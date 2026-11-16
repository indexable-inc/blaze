//! A fn's body, ready for the check loop (step 2's rest): its text lowered by blaze-lower's
//! `lower_body_expanding`, each macro call resolved by the names library (blaze-lower's
//! [`Scoped`] over `Names::macro_definition`), then converted by `to_typeck` with its written
//! types built by [`Builder`] into the store's table. A head the builder needs first
//! ([`Why::Need`]) is made by [`Store::ensure`] and the body converted again, at most
//! [`MOST_ROUNDS`] times. Its [`BodyScope`]: the fn's module, its generic parameters (from its
//! signature) and the traits in scope there (the names library's `traits_in_scope`).
//!
//! Taken now: a free fn (an item with text of its own). A fn of a trait or an impl is a member,
//! with no item text in the names library: [`BodyError::NoText`] names it (its parent's generics
//! and `Self` are the next piece).
//!
//! The expansion's host refuses every outside read (`env!`, `cfg!`, `include!`, a proc macro):
//! the crate's environment and cfg reach a body at the switch; until then such a call is a
//! finding of the lowering, by its range.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::vec::Vec;

use blaze_expand::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream};
use blaze_id::ItemKey as LibKey;
use blaze_lower::by_name::ByName;
use blaze_lower::form as lf;
use blaze_lower::scoped::{Prepares, Scoped};
use blaze_lower::typeck::{Refused, TypeckBody, Types, to_typeck};
use blaze_lower::{Lowered, View};
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_surface::Range;
use blaze_typeck::{ExprId, ItemKey, Name, PTy, PortableTys};

use super::check::{BodyScope, MOST_ROUNDS};
use super::heads::{CrateCfg, FnHead, HeadError, Maps, Store, Want, own_generics};
use super::keys::{Keys, checker_of};
use super::spell::Spelling;
use super::types::{Bounds, Builder, Env, ParamBound, SelfTy, Why};
use crate::resolve::Resolver;

/// What a body's lowering did not take, by its range in the item's text (or past it, in an
/// expansion appended to it).
#[derive(Debug)]
pub enum BodyFinding {
    /// A macro call whose path names no macro the expander could run. Its path and why are
    /// rendered: the expander's finding borrows its frame, which ends with the lowering.
    Macro { at: Range, path: Box<str, Global>, why: Box<str, Global> },
    /// A construct blaze-lower does not lower yet.
    Unhandled { syntax: blaze_file_items::SyntaxKind, why: lf::Why, range: Range },
    /// An error the lowering found in the text.
    Diag { kind: lf::DiagKind, range: Range },
    /// The fn is turned off by a `cfg`.
    CfgOut,
}

/// Why a fn's body is not ready for the check loop.
#[derive(Debug)]
pub enum BodyError {
    /// The names library holds no text for the item (a member of a trait or an impl, a
    /// variant), or no crate for its key.
    NoText(LibKey),
    /// Its lowering asked for a `cfg` the crate's atoms do not answer.
    Cfg(LibKey),
    /// Its generic parameters could not be read from its signature.
    Generics { item: LibKey, why: Why },
    /// Its lowering's record is malformed (a row, a range).
    Record { item: LibKey, why: Why },
    /// What its lowering did not take, in the order found: expander, then constructs, then
    /// errors.
    Lowered { item: LibKey, findings: Vec<BodyFinding, Global> },
    /// `to_typeck` refused it; `why` is the type builder's reason when a written type did.
    Convert { item: LibKey, refused: Refused, why: Option<Why> },
    /// A head the body's types need could not be made.
    Head { item: LibKey, error: HeadError },
    /// Still needing heads after [`MOST_ROUNDS`] conversions.
    Rounds { item: LibKey, last: LibKey },
    /// The body's own type table (a copy of the records' table) could not be made.
    Table { item: LibKey, error: blaze_typeck::CheckError },
    /// The traits in scope at its module could not be read.
    Traits { item: LibKey, error: blaze_scope::names::PathError },
    /// A member fn whose head the store has not made (its container's head is made first).
    NoHead(ItemKey),
    /// A member fn with no body (a required trait method): nothing to check.
    NoBody(ItemKey),
    /// The constant `item` an array length of the body (or of a head it needs) names has no
    /// value: the evaluator's refusal.
    Const { item: LibKey, error: Box<super::consts::ConstFail, Global> },
}

impl core::fmt::Display for BodyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BodyError::Const { item, error } => write!(f, "constant {item:x}, an array length: {error}"),
            BodyError::NoText(k) => write!(f, "fn {k:x}: the names library holds no text for it (a member of a trait or an impl is lowered by `lower_member`)"),
            BodyError::NoHead(k) => write!(f, "member fn {k:?}: its head is not in the store (make its container's head first)"),
            BodyError::NoBody(k) => write!(f, "member fn {k:?}: it has no body (a required trait method)"),
            BodyError::Cfg(k) => write!(f, "fn {k:x}: its lowering asks for a `cfg` the crate does not answer"),
            BodyError::Generics { item, why } => write!(f, "fn {item:x}: its generic parameters: {why:?}"),
            BodyError::Record { item, why } => write!(f, "fn {item:x}: its lowered record: {why:?}"),
            BodyError::Lowered { item, findings } => {
                write!(f, "fn {item:x}: {} things its lowering did not take", findings.len())?;
                for x in findings {
                    write!(f, "\n  {x:?}")?;
                }
                Ok(())
            }
            BodyError::Convert { item, refused, why } => write!(f, "fn {item:x}: to_typeck refused {refused:?} ({why:?})"),
            BodyError::Head { item, error } => write!(f, "fn {item:x}: {error}"),
            BodyError::Rounds { item, last } => write!(f, "fn {item:x}: still needs the head of {last:x} after {MOST_ROUNDS} conversions"),
            BodyError::Traits { item, error } => write!(f, "fn {item:x}: the traits in scope at its module: {error}"),
            BodyError::Table { item, error } => write!(f, "fn {item:x}: its own type table: {error:?}"),
        }
    }
}

/// A fn's body converted for the checker, its types in the store's table.
pub struct Body<'n> {
    pub item: LibKey,
    pub owner: ItemKey,
    pub module: LibKey,
    /// The body's lowering: its view spells the body's names ([`Spelling::of`]).
    pub lowered: Lowered<'static, 'n>,
    pub typeck: TypeckBody<'static>,
    /// The body's types: a copy of the records' table (`Store::tys`) when it was lowered, then
    /// the body's own (closures, futures, holes). A record is answered from `Store::tys`
    /// alone, which never holds a body's type, whatever bodies were checked before.
    pub tys: PortableTys<'static>,
    pub params: Vec<(Name, u32), Global>,
    pub self_: SelfTy,
    /// The traits in scope at `module`, checker keys, sorted.
    pub traits: Vec<ItemKey, Global>,
}

impl<'n> Body<'n> {
    /// The scope the check loop resolves the body's paths in; `spell` is
    /// `Spelling::of(&self.lowered.view())`.
    pub fn scope<'s>(&'s self, spell: &'s Spelling<'s>) -> BodyScope<'s> {
        BodyScope { module: self.module, params: &self.params, self_: &self.self_, spell, traits: &self.traits }
    }
}

/// The expansion's host while no crate environment reaches a body: every read refused, by
/// what it asked.
pub(super) struct NoHost;

fn refused(ask: HostAsk) -> HostError {
    HostError { ask, why: HostWhy::Refused }
}

impl ExpandCx for NoHost {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Err(refused(HostAsk::Env))
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Err(refused(HostAsk::Cfg))
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Err(refused(HostAsk::Include))
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Err(refused(HostAsk::Place))
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(refused(HostAsk::ProcMacro))
    }
}

/// A body's written types: built by [`Builder`] where its fn's names hold; the builder's
/// reason kept, since `to_typeck`'s refusal has no room for it.
struct BodyTypes<'b, 'n> {
    builder: Builder<'b, 'n, Maps>,
    env: Env<'b>,
    failed: Option<Why>,
}

impl Types for BodyTypes<'_, '_> {
    fn ty(&mut self, view: &View<'_>, t: lf::TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        match self.builder.ty(tys, &self.env, view, t) {
            Ok(ty) => Ok(ty),
            Err(why) => {
                self.failed = Some(why);
                Err(Refused::Form { what: "a written type", range: view.site(view.type_range(t)?)? })
            }
        }
    }

    /// `[x; N]`'s length, as a written array type's ([`Builder::array_len`]): a literal, or a
    /// constant item's value once evaluated ([`Why::NeedConst`] until then).
    fn repeat_len(&mut self, view: &View<'_>, len: ExprId) -> Result<u64, Refused> {
        let at = view.expr_range(len)?;
        let range = view.site(at)?;
        let text = view.text_at(at)?.trim();
        match self.builder.array_len(&self.env, text, range) {
            Ok(n) => u64::try_from(n).map_err(|_| Refused::Form { what: "an array repeat length past `usize`", range }),
            Err(why) => {
                self.failed = Some(why);
                Err(Refused::Form { what: "an array repeat length", range })
            }
        }
    }

    /// A const argument that is no literal, read by the one home for written constants
    /// ([`Builder::const_term`]: `{ e }` as `e`, a const parameter, a constant item's value;
    /// its type left to the checker, which takes the parameter's), except a path to an enum's
    /// variant (`{ AtomicOrdering::SeqCst }`, `adt_const_params`), which is that variant's
    /// value ([`PortableTys::variant_value`]). OWED: a variant of a generic enum, or one with
    /// fields, is taken as a unit variant of a plain enum; the checker's equality with the
    /// parameter's type is what refuses it.
    fn const_arg(&mut self, view: &View<'_>, c: lf::ConstIdx, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        let at = view.const_range(c)?;
        let range = view.site(at)?;
        let mut text = view.text_at(at)?.trim();
        if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            text = inner.trim();
        }
        let variant = match text.split_once("::") {
            Some(_) => {
                let (abs, rest) = match text.strip_prefix("::") {
                    Some(rest) => (true, rest),
                    None => (false, text),
                };
                let mut segs: Vec<&str, Global> = Vec::new_in(Global);
                segs.extend(rest.split("::").map(str::trim));
                match self.builder.resolver.path(self.env.module, abs, &segs, crate::resolve::Ns::Value) {
                    Ok(found) if found.consumed as usize == segs.len() => match found.kind {
                        crate::resolve::Kind::Variant { adt, index } => Some((adt, index)),
                        _ => None,
                    },
                    _ => None,
                }
            }
            None => None,
        };
        let made = match variant {
            Some((adt, index)) => tys.variant_value(checker_of(adt), index).map_err(|e| Why::Program(format!("the type table refused a variant's value: {e:?}").into_boxed_str(), range)),
            None => self.builder.const_term(tys, &self.env, text, range, &|tys| super::types::intern(tys, blaze_typeck::PNode::Infer)),
        };
        match made {
            Ok(t) => Ok(t),
            Err(why) => {
                self.failed = Some(why);
                Err(Refused::Form { what: "a const argument", range })
            }
        }
    }
}

/// Lowers and converts the body of the free fn `item`, making the heads its written types need.
/// `prepared` is the build's one set of prepared macro definitions (each kept by its item's
/// key), shared by every body the build lowers.
pub fn lower_fn<'n>(names: &'n Names<'n>, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, item: LibKey) -> Result<Body<'n>, BodyError> {
    lower_fn_into(names, prepared, keys, store, item, None)
}

/// Lowers `body`'s fn again, its types into `into` (a copy of `store.tys` made after every
/// head it needs: a head it would still need is a refusal), for a driver that reads every
/// body and record from one table (the run path's seam). The returned body's own `tys` is
/// empty: its types are `into`'s.
pub fn lower_again<'n>(names: &'n Names<'n>, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, body: &Body<'n>, into: &mut PortableTys<'static>) -> Result<Body<'n>, BodyError> {
    match keys.library(body.owner) {
        Some(lib) => lower_fn_into(names, prepared, keys, store, lib, Some(into)),
        None => lower_member_into(names, prepared, keys, store, body.owner, Some(into)),
    }
}

fn lower_fn_into<'n>(names: &'n Names<'n>, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, item: LibKey, into: Option<&mut PortableTys<'static>>) -> Result<Body<'n>, BodyError> {
    let it = names.item_text(item).ok_or(BodyError::NoText(item))?;
    let krate = names.crate_of(item).ok_or(BodyError::NoText(item))?;
    let (module, text) = (it.module, it.text);
    let atoms = names.krate(krate).cfg.atoms();
    let sig = blaze_lower::lower_signature(text, names.edition(krate), &mut CrateCfg(atoms), Alloc::global()).map_err(|_| BodyError::Cfg(item))?;
    let mut params = Vec::new_in(Global);
    let mut consts = Vec::new_in(Global);
    let b = Builder { resolver: Resolver { names }, heads: &store.maps };
    own_generics(&b, &mut store.tys, module, &sig.view(), 0, &mut params, &mut consts).map_err(|why| BodyError::Generics { item, why })?;
    let mut bounds = Vec::new_in(Global);
    let env = Env { module, params: &params, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &consts };
    b.param_bounds(&mut store.tys, &env, &sig.view(), 0, &mut bounds).map_err(|why| BodyError::Generics { item, why })?;
    let site = Site { item, owner: checker_of(item), krate, module, text, params, consts, bounds, self_: SelfTy::None, self_assoc: Vec::new_in(Global) };
    lower_at(names, prepared, keys, store, site, into)
}

/// Lowers and converts the body of the member fn `key` of a trait or an impl, in the env its
/// head was made in ([`FnHead`]: the Reference's rule, Items > Generics and Implementations:
/// the container's generic parameters are in scope in its associated items, before the item's
/// own; `Self` is the implementing type in an impl, and the trait's implicit type parameter,
/// index 0, in a trait). Its head must be made: a callee the checker picked always has one.
pub fn lower_member<'n>(names: &'n Names<'n>, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, key: ItemKey) -> Result<Body<'n>, BodyError> {
    lower_member_into(names, prepared, keys, store, key, None)
}

fn lower_member_into<'n>(names: &'n Names<'n>, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, key: ItemKey, into: Option<&mut PortableTys<'static>>) -> Result<Body<'n>, BodyError> {
    let head: &FnHead<'n> = store.fns.get(&key).ok_or(BodyError::NoHead(key))?;
    if !head.has_body {
        return Err(BodyError::NoBody(key));
    }
    let krate = names.crate_of(head.lib).ok_or(BodyError::NoText(head.lib))?;
    let site = Site {
        item: head.lib,
        owner: key,
        krate,
        module: head.module,
        text: head.text,
        params: head.params.clone(),
        consts: head.consts.clone(),
        bounds: head.bounds.clone(),
        self_: head.self_,
        self_assoc: head.self_assoc.clone(),
    };
    lower_at(names, prepared, keys, store, site, into)
}

/// Where a body is lowered: its item (a member's is its container), its checker key, text and
/// the names its types are read with.
struct Site<'n> {
    item: LibKey,
    owner: ItemKey,
    krate: usize,
    module: LibKey,
    text: &'n str,
    params: Vec<(Name, u32), Global>,
    /// The const generic parameters among `params` ([`Env::consts`]).
    consts: Vec<(u32, PTy), Global>,
    bounds: Vec<ParamBound, Global>,
    self_: SelfTy,
    self_assoc: Vec<(Name, PTy), Global>,
}

fn lower_at<'n>(names: &'n Names<'n>, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, site: Site<'n>, mut into: Option<&mut PortableTys<'static>>) -> Result<Body<'n>, BodyError> {
    let Site { item, owner, krate, module, text, params, consts, bounds, self_, self_assoc } = site;
    let edition = names.edition(krate);
    let atoms = names.krate(krate).cfg.atoms();

    let scoped = Scoped { names, module, prepared: Some(prepared) };
    let mut expand = ByName::with_names(&scoped, NoHost, Alloc::global());
    let lowered = blaze_lower::lower_body_expanding(text, edition, &mut CrateCfg(atoms), &mut expand, Alloc::global()).map_err(|_| BodyError::Cfg(item))?;
    let view = lowered.view();
    let mut findings = Vec::new_in(Global);
    for m in expand.findings() {
        findings.push(BodyFinding::Macro { at: m.at, path: Box::from(m.path.as_str()), why: format!("{}", m.why).into_boxed_str() });
    }
    let record = |why: Why| BodyError::Record { item, why };
    for u in view.unhandled() {
        let range = view.site(u.range).map_err(|t| record(Why::from(t)))?;
        findings.push(BodyFinding::Unhandled { syntax: blaze_file_items::syntax_kind(u.syntax), why: u.why, range });
    }
    for d in view.diags() {
        let d = d.map_err(|b| record(Why::from(b)))?;
        let range = view.site(d.range).map_err(|t| record(Why::from(t)))?;
        findings.push(BodyFinding::Diag { kind: d.kind, range });
    }
    if view.cfg_out() {
        findings.push(BodyFinding::CfgOut);
    }
    if !findings.is_empty() {
        return Err(BodyError::Lowered { item, findings });
    }

    let mut last = item;
    let mut typeck = None;
    for _ in 0..MOST_ROUNDS {
        let mut types = BodyTypes {
            builder: Builder { resolver: Resolver { names }, heads: &store.maps },
            env: Env { module, params: &params, bounds: Bounds::Done(&bounds), self_, self_assoc: &self_assoc, anon: &[], consts: &consts },
            failed: None,
        };
        // Each round converts into a fresh copy of the records' table: a refused round's
        // types are dropped with it, and none reaches `store.tys`.
        let (mut own, again) = match into.as_deref_mut() {
            Some(t) => (PortableTys::new_in(Alloc::global()), Some(t)),
            None => (store.tys.fork_in(Alloc::global()).map_err(|error| BodyError::Table { item, error })?, None),
        };
        let again_now = again.is_some();
        let tys = match again {
            Some(t) => t,
            None => &mut own,
        };
        match to_typeck(&view, owner, tys, &mut types, Alloc::global()) {
            Ok(b) => {
                typeck = Some((b, own));
                break;
            }
            Err(refused) => match types.failed.take() {
                // Lowered again into a finished table: a head it lacks cannot be added there.
                Some(why @ Why::Need(_)) if again_now => return Err(BodyError::Convert { item, refused, why: Some(why) }),
                Some(Why::Need(k)) => {
                    last = k;
                    let need = store.need(k);
                    store.ensure(names, keys, need).map_err(|error| BodyError::Head { item, error })?;
                }
                why => return Err(BodyError::Convert { item, refused, why }),
            },
        }
    }
    let Some((typeck, tys)) = typeck else { return Err(BodyError::Rounds { item, last }) };

    let in_scope = names.traits_in_scope(module, Alloc::global()).map_err(|error| BodyError::Traits { item, error })?;
    let mut traits = Vec::with_capacity_in(in_scope.len(), Global);
    traits.extend(in_scope.iter().map(|&k| checker_of(k)));
    traits.sort_unstable();
    traits.dedup();
    drop(view);
    Ok(Body { item, owner, module, lowered, typeck, tys, params, self_, traits })
}
