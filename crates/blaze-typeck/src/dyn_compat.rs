//! Dyn compatibility (the Reference, "Traits", "Dyn compatibility"): the one home of the rule.
//! A world states facts about each member of a trait ([`MemberFacts`]); this module decides.
//! Wiring (typeck.md, "Trait objects" (e)): `TraitDef` carries the members, the verdict over
//! the principal and its supertraits sets the solver's `DYN_COMPATIBLE`, and a `dyn` written in
//! a body reports [`DynRule`] once per trait per body.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::{ItemKey, LangItem, Name};
use crate::portable::{PNode, PTy, PTys};
use crate::world::{PClause, Question, Reads, TraitMembers, World};

/// How a method takes `self`, as written (the Reference, "Associated Items", "Methods").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Receiver {
    /// An associated function without `self`.
    Absent,
    /// `self` (by value: `Self`).
    Value,
    /// `&self`.
    Ref,
    /// `&mut self`.
    RefMut,
    /// `self: Box<Self>`.
    Box,
    /// `self: Rc<Self>`.
    Rc,
    /// `self: Arc<Self>`.
    Arc,
    /// `self: Pin<P>`, `P` one of the four pointer forms above.
    PinOfPointer,
    /// Any other receiver type.
    Other,
}

/// What a world could not read of a member: never taken as compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unread {
    /// A receiver written `self: T` whose `T` the world does not classify.
    TypedReceiver,
    /// A type in the signature the world does not walk.
    SignatureType,
}

/// One associated function's facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FnFacts {
    pub receiver: Receiver,
    /// Its own generic parameters that are not lifetimes (types and consts).
    pub own_non_lifetime_params: u32,
    /// A `where Self: Sized` bound of its own.
    pub where_self_sized: bool,
    /// `async fn`.
    pub is_async: bool,
    /// A return-position `impl Trait`.
    pub opaque_return: bool,
    /// `Self` (other than as a projection's self type, `Self::X`) in a parameter after the
    /// receiver or in the return type.
    pub self_outside_receiver: bool,
}

/// One member's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberShape {
    Const,
    /// An associated type; `generic`: it has generic parameters of its own (lifetimes count).
    Type { generic: bool },
    Fn(FnFacts),
    Unread(Unread),
}

/// One member of a trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemberFacts {
    pub name: Name,
    pub shape: MemberShape,
}

/// The rule a trait breaks, each quoting the Reference ("Dyn compatibility").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DynRule {
    /// "`Sized` must not be a supertrait."
    SizedSupertrait,
    /// "It must not have any associated constants."
    AssocConst,
    /// "It must not have any associated types with generics."
    GenericAssocType,
    /// Dispatchable: "Be a method" (an associated function without `self` is neither
    /// dispatchable nor, without `where Self: Sized`, explicitly non-dispatchable).
    NoReceiver,
    /// Dispatchable: "Have a receiver with one of the following types: `&Self`, `&mut Self`,
    /// `Box<Self>`, `Rc<Self>`, `Arc<Self>`, `Pin<P>` where `P` is one of the types above".
    Receiver,
    /// Dispatchable: "Not have any type parameters (although lifetime parameters are allowed)".
    TypeParams,
    /// Dispatchable: "does not use `Self` except in the type of the receiver".
    SelfOutsideReceiver,
    /// Dispatchable: "Not have an opaque return type; that is, Not be an `async fn`".
    AsyncFn,
    /// Dispatchable: "not have a return-position `impl Trait` type".
    OpaqueReturn,
}

impl DynRule {
    /// The Reference's words for the rule.
    pub const fn quote(self) -> &'static str {
        match self {
            DynRule::SizedSupertrait => "`Sized` must not be a supertrait",
            DynRule::AssocConst => "it must not have any associated constants",
            DynRule::GenericAssocType => "it must not have any associated types with generics",
            DynRule::NoReceiver => "a dispatchable function must be a method",
            DynRule::Receiver => "a dispatchable function's receiver must be `&Self`, `&mut Self`, `Box<Self>`, `Rc<Self>`, `Arc<Self>` or `Pin<P>` of them",
            DynRule::TypeParams => "a dispatchable function must not have any type parameters",
            DynRule::SelfOutsideReceiver => "a dispatchable function must not use `Self` except in the type of the receiver",
            DynRule::AsyncFn => "a dispatchable function must not be an `async fn`",
            DynRule::OpaqueReturn => "a dispatchable function must not have a return-position `impl Trait` type",
        }
    }
}

/// One member's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberVerdict {
    Compatible,
    Not(DynRule),
    /// The world could not read it: a named gap, never compatible.
    Gap(Unread),
}

#[cfg(test)]
std::thread_local! {
    /// Planted bug for the test's control: every member compatible (today's state before the
    /// rule: nothing set `NOT_DYN_COMPATIBLE`).
    pub(crate) static PLANT_ALL_COMPATIBLE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// One member's verdict (the Reference, "Dyn compatibility"). "Explicitly non-dispatchable
/// functions require: Have a `where Self: Sized` bound (receiver type of `Self` (i.e. `self`)
/// implies this)": such a function is allowed and not callable on the object.
pub fn member_verdict(m: &MemberFacts) -> MemberVerdict {
    #[cfg(test)]
    if PLANT_ALL_COMPATIBLE.with(core::cell::Cell::get) {
        return MemberVerdict::Compatible;
    }
    let f = match m.shape {
        MemberShape::Const => return MemberVerdict::Not(DynRule::AssocConst),
        MemberShape::Type { generic: true } => return MemberVerdict::Not(DynRule::GenericAssocType),
        MemberShape::Type { generic: false } => return MemberVerdict::Compatible,
        MemberShape::Unread(u) => return MemberVerdict::Gap(u),
        MemberShape::Fn(f) => f,
    };
    if f.where_self_sized || f.receiver == Receiver::Value {
        return MemberVerdict::Compatible;
    }
    let broken = match f.receiver {
        Receiver::Absent => Some(DynRule::NoReceiver),
        Receiver::Other => Some(DynRule::Receiver),
        _ if f.own_non_lifetime_params != 0 => Some(DynRule::TypeParams),
        _ if f.self_outside_receiver => Some(DynRule::SelfOutsideReceiver),
        _ if f.is_async => Some(DynRule::AsyncFn),
        _ if f.opaque_return => Some(DynRule::OpaqueReturn),
        _ => None,
    };
    match broken {
        Some(r) => MemberVerdict::Not(r),
        None => MemberVerdict::Compatible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn f(receiver: Receiver) -> FnFacts {
        FnFacts {
            receiver,
            own_non_lifetime_params: 0,
            where_self_sized: false,
            is_async: false,
            opaque_return: false,
            self_outside_receiver: false,
        }
    }

    fn m(name: &str, shape: MemberShape) -> MemberFacts {
        MemberFacts { name: Name::of(name), shape }
    }

    /// The cases: (members, `Sized` a supertrait, expected verdict).
    fn cases() -> std::vec::Vec<(std::vec::Vec<MemberFacts>, bool, Result<(), (Option<Name>, MemberVerdict)>)> {
        let ok = |name: &str, x: FnFacts| m(name, MemberShape::Fn(x));
        std::vec![
            // Dispatchable receivers, and `Self::X` (a projection) in the return type.
            (std::vec![ok("a", f(Receiver::Ref)), ok("b", f(Receiver::RefMut)), ok("c", f(Receiver::Box)), ok("d", f(Receiver::Rc)), ok("e", f(Receiver::Arc)), ok("p", f(Receiver::PinOfPointer)), m("Item", MemberShape::Type { generic: false })], false, Ok(())),
            // "receiver type of `Self` (i.e. `self`) implies" `where Self: Sized`.
            (std::vec![ok("into", f(Receiver::Value))], false, Ok(())),
            // Explicitly non-dispatchable: generic, `Self` elsewhere, no receiver, async.
            (std::vec![ok("g", FnFacts { own_non_lifetime_params: 1, where_self_sized: true, ..f(Receiver::Ref) }), ok("new", FnFacts { where_self_sized: true, ..f(Receiver::Absent) })], false, Ok(())),
            (std::vec![], true, Err((None, MemberVerdict::Not(DynRule::SizedSupertrait)))),
            (std::vec![m("N", MemberShape::Const)], false, Err((Some(Name::of("N")), MemberVerdict::Not(DynRule::AssocConst)))),
            (std::vec![m("G", MemberShape::Type { generic: true })], false, Err((Some(Name::of("G")), MemberVerdict::Not(DynRule::GenericAssocType)))),
            (std::vec![ok("new", f(Receiver::Absent))], false, Err((Some(Name::of("new")), MemberVerdict::Not(DynRule::NoReceiver)))),
            (std::vec![ok("v", f(Receiver::Other))], false, Err((Some(Name::of("v")), MemberVerdict::Not(DynRule::Receiver)))),
            (std::vec![ok("g", FnFacts { own_non_lifetime_params: 1, ..f(Receiver::Ref) })], false, Err((Some(Name::of("g")), MemberVerdict::Not(DynRule::TypeParams)))),
            (std::vec![ok("eq", FnFacts { self_outside_receiver: true, ..f(Receiver::Ref) })], false, Err((Some(Name::of("eq")), MemberVerdict::Not(DynRule::SelfOutsideReceiver)))),
            (std::vec![ok("run", FnFacts { is_async: true, ..f(Receiver::Ref) })], false, Err((Some(Name::of("run")), MemberVerdict::Not(DynRule::AsyncFn)))),
            (std::vec![ok("it", FnFacts { opaque_return: true, ..f(Receiver::Ref) })], false, Err((Some(Name::of("it")), MemberVerdict::Not(DynRule::OpaqueReturn)))),
            (std::vec![m("x", MemberShape::Unread(Unread::TypedReceiver))], false, Err((Some(Name::of("x")), MemberVerdict::Gap(Unread::TypedReceiver)))),
        ]
    }

    /// The Reference, "Traits", "Dyn compatibility": "All supertraits must also be dyn
    /// compatible. `Sized` must not be a supertrait. It must not have any associated constants.
    /// It must not have any associated types with generics. All associated functions must
    /// either be dispatchable from a trait object or be explicitly non-dispatchable"; each
    /// dispatchable rule and the `where Self: Sized` exemption, one case each.
    #[test]
    fn dyn_compat_each_rule() {
        for (members, sized, want) in cases() {
            assert_eq!(own_verdict(sized, &members), want, "{members:?}");
        }
    }

    /// Control: with the planted bug (every trait compatible, the state before this rule) the
    /// cases that must be refused are accepted, so `dyn_compat_each_rule` catches it.
    #[test]
    fn control_dyn_compat_all_compatible() {
        PLANT_ALL_COMPATIBLE.with(|p| p.set(true));
        let missed = cases().into_iter().filter(|(ms, sized, want)| own_verdict(*sized, ms) != *want).count();
        PLANT_ALL_COMPATIBLE.with(|p| p.set(false));
        assert_eq!(missed, 10, "the plant must break every refusing case");
    }
}

/// A trait's own verdict from its members, in order: the first member not compatible, by name.
/// `sized_supertrait`: its supertraits include the `Sized` lang item. Supertraits are walked by
/// the caller (each must be compatible too: "All supertraits must also be dyn compatible").
pub fn own_verdict(sized_supertrait: bool, members: &[MemberFacts]) -> Result<(), (Option<Name>, MemberVerdict)> {
    if sized_supertrait {
        #[cfg(test)]
        if PLANT_ALL_COMPATIBLE.with(core::cell::Cell::get) {
            return Ok(());
        }
        return Err((None, MemberVerdict::Not(DynRule::SizedSupertrait)));
    }
    for m in members {
        match member_verdict(m) {
            MemberVerdict::Compatible => {}
            v => return Err((Some(m.name), v)),
        }
    }
    Ok(())
}

/// The most traits the verdict walks above one principal (rule 18).
pub const MAX_SUPERTRAITS: usize = 64;

/// Why a verdict is a gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GapWhy {
    /// The world has not read the trait's members (`TraitMembers::NotKnown`).
    MembersNotKnown,
    /// A member the world could not read.
    Member(Name, Unread),
    /// A supertrait's arguments name `Self` (`trait A: B<Self>`): rustc refuses it; the
    /// Reference does not list it, so it is neither compatible nor refused here.
    SelfInSupertraitArgs,
}

/// A trait's verdict with its supertraits ("All supertraits must also be dyn compatible").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    Compatible,
    /// `by` (the trait or a supertrait) breaks `rule`, at `member` (`None`: `Sized` above it).
    Not { trait_: ItemKey, by: ItemKey, member: Option<Name>, rule: DynRule },
    Gap { trait_: ItemKey, by: ItemKey, why: GapWhy },
}

/// The verdict of `trait_` and every trait above it, through a fixed worklist of at most
/// [`MAX_SUPERTRAITS`] traits (a supertrait is a `PClause::Trait` on `Param(0)`); a supertrait
/// that is the `Sized` lang item is [`DynRule::SizedSupertrait`].
pub(crate) fn verdict<W: World>(reads: &mut Reads<'_, '_, W>, trait_: ItemKey) -> Result<Verdict, Stop> {
    let alloc = reads.alloc();
    let mut list = [ItemKey(0); MAX_SUPERTRAITS];
    list[0] = trait_;
    let mut len = 1usize;
    let mut at = 0usize;
    while at < len {
        let t = list[at];
        at += 1;
        let Some(def) = reads.trait_def(t)? else {
            return Err(CheckError::Malformed(Malformed::NoRecord(Question::TraitDef(t))).into());
        };
        // `dyn Sized`: the trait is its own supertrait (rustc's `trait_has_sized_self` walks the
        // elaborated bounds, the trait's own `Self: Sized` first).
        if def.lang == Some(LangItem::Sized) {
            return Ok(Verdict::Not { trait_, by: t, member: None, rule: DynRule::SizedSupertrait });
        }
        let mut sized = false;
        for clause in def.supertraits {
            let PClause::Trait { trait_: st, args } = *clause else { continue };
            let list_args = def.types.list(args)?;
            let Some((&first, rest)) = list_args.split_first() else { continue };
            if def.types.node(first)? != PNode::Param(0) {
                continue;
            }
            if names_self(def.types, rest, alloc)? {
                return Ok(Verdict::Gap { trait_, by: t, why: GapWhy::SelfInSupertraitArgs });
            }
            if reads.trait_def(st)?.is_some_and(|d| d.lang == Some(LangItem::Sized)) {
                sized = true;
                continue;
            }
            if list[..len].contains(&st) {
                continue;
            }
            if len == MAX_SUPERTRAITS {
                return Err(CheckError::Limit(Limit::Supertraits).into());
            }
            list[len] = st;
            len += 1;
        }
        let members = match def.members {
            TraitMembers::Known(ms) => ms,
            TraitMembers::NotKnown => return Ok(Verdict::Gap { trait_, by: t, why: GapWhy::MembersNotKnown }),
        };
        match own_verdict(sized, members) {
            Ok(()) => {}
            Err((member, MemberVerdict::Not(rule))) => return Ok(Verdict::Not { trait_, by: t, member, rule }),
            Err((Some(name), MemberVerdict::Gap(u))) => return Ok(Verdict::Gap { trait_, by: t, why: GapWhy::Member(name, u) }),
            Err((None, _)) | Err((_, MemberVerdict::Compatible)) => {
                return Err(CheckError::Malformed(Malformed::NoRecord(Question::TraitDef(t))).into());
            }
        }
    }
    Ok(Verdict::Compatible)
}

/// E0038 for each `dyn` written in item `item`'s own types, an ADT's fields or a fn's
/// signature, that are not inside a body (the Reference, "Trait objects": "the trait must be
/// dyn compatible", wherever the type is written; rustc's `check_type_wf` does this per item).
/// A body's written types and its owner's signature are checked by the body's walk
/// (`Machine::check_dyn`); this is for the items no body checks: a struct's fields, a
/// bodiless fn (a trait's method declaration, a foreign fn). One finding per trait per item,
/// with an empty range (`DiagCode::covers_the_item`), in the order the traits are first met.
/// A fact the world lacks is `Stop::Need`; a trait it cannot decide is `CheckError::
/// DynUndecided`, never compatible.
pub fn item_dyn_findings<W: World>(world: &W, item: ItemKey, out: &mut Vec<crate::diag::Diag, Alloc<'_>>) -> Result<(), Stop> {
    // The caller's allocator, the one `out` lives in (D102).
    let alloc = *out.allocator();
    let mut reads = Reads::new_in(world, alloc);
    let mut roots: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    let types = if let Some(adt) = reads.adt(item)? {
        roots.extend(adt.fields.iter().map(|f| f.ty));
        adt.types
    } else if let Some(sig) = reads.fn_sig(item)? {
        roots.extend_from_slice(sig.inputs);
        roots.push(sig.output);
        sig.types
    } else {
        return Err(CheckError::Malformed(Malformed::NoRecord(Question::FnSig(item))).into());
    };
    // `len_now`: a live view's later types are walked too (portable.rs `PTys`).
    let mut seen_nodes: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(types.len_now(), alloc);
    seen_nodes.resize(types.len_now(), false);
    let mut traits: Vec<ItemKey, Alloc<'_>> = Vec::new_in(alloc);
    let mut kids: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    // Children are pushed reversed so the walk meets types in written order.
    roots.reverse();
    while let Some(p) = roots.pop() {
        let Some(s) = seen_nodes.get_mut(p.0 as usize) else {
            return Err(CheckError::Malformed(Malformed::TypeOrder { node: p.0, child: p.0 }).into());
        };
        if core::mem::replace(s, true) {
            continue;
        }
        let n = types.node(p)?;
        if let PNode::Dynamic(l) = n
            && let Some(principal) = crate::portable::object_principal(types, l)
            && !traits.contains(&principal)
        {
            traits.push(principal);
            match verdict(&mut reads, principal)? {
                Verdict::Compatible => {}
                Verdict::Not { trait_, by, member, rule } => {
                    out.push(crate::diag::Diag::dyn_incompatible(crate::diag::Because::Dyn { trait_, by, member, rule }, member));
                }
                Verdict::Gap { trait_, by, why } => return Err(CheckError::DynUndecided { trait_, by, why }.into()),
            }
        }
        kids.clear();
        crate::machine::children(n, types, &mut kids)?;
        roots.extend(kids.iter().rev());
    }
    Ok(())
}

/// Whether `Param(0)` is in any of the types `l` of `t`: a bounded walk over the node table,
/// whose children come before their parents (each node is visited once).
pub(crate) fn names_self(t: PTys<'_>, l: &[PTy], alloc: Alloc<'_>) -> Result<bool, Stop> {
    let mut seen: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(t.len(), alloc);
    seen.resize(t.len(), false);
    let mut todo: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(l.len(), alloc);
    todo.extend_from_slice(l);
    let mut kids: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    while let Some(p) = todo.pop() {
        let Some(s) = seen.get_mut(p.0 as usize) else { return Err(CheckError::Malformed(Malformed::TypeOrder { node: p.0, child: p.0 }).into()) };
        if core::mem::replace(s, true) {
            continue;
        }
        let n = t.node(p)?;
        if n == PNode::Param(0) {
            return Ok(true);
        }
        kids.clear();
        crate::machine::children(n, t, &mut kids)?;
        todo.extend_from_slice(&kids);
    }
    Ok(false)
}
