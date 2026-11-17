//! Composed grants: a small algebra over [`Grant`], each form readable from the binary's
//! manifest without running the program (what it NEEDS, what it merely CAN USE, and its
//! ALTERNATIVES).
//!
//! | Written | Means | Manifest |
//! |---|---|---|
//! | `data: Dir<ReadOnly>` | needed: no grant is a refusal | (as before: no `form` key) |
//! | `cache: Maybe<Dir<ReadWrite>>` | can use: `None` when absent | `"form":"optional"` |
//! | `store: OneOf<(Dir<ReadWrite>, Net)>` | the first alternative granted, in written order | `"form":"one_of","alternatives":[..]` |
//! | `inputs: Many<Dir<ReadOnly>>` | every grant `inputs.<member>`, in name order; zero allowed | `"form":"many","min":0` |
//! | `api: Connect` / `feed: Listen` | one endpoint, in one direction | `"kind":"net","direction":"connect"` |
//!
//! - [`Maybe<G>`]: **an absent grant is never a refusal, a wrong one always is.** No grant of
//!   its name, or a host that does not offer the capability, is `None`; a grant that IS given
//!   and is wrong (another kind, read only for a writer, a folder that cannot be opened) refuses
//!   the start as it would for `G`: the granter meant something, and a program that silently
//!   ran without it would hide the mistake. It is a bstd type, not `Option<G>`: a test's drawn
//!   `Option<u32>` is made by the test engine's blanket impl (simtest/param.rs), and an
//!   `Option<G>` capability beside it would overlap (E0119: `Option` is not a local type), so the
//!   two would need a second mechanism to tell apart; `Maybe` needs none, and reads as what it is.
//!   [`Maybe::get`] and [`Maybe::into_option`] give the `Option`.
//! - [`OneOf<(A, B)>`] (up to four): named for what the TYPE says (one of these), not `Select`,
//!   which reads as a choice the program makes at run time. Its value is [`Chosen2`] (`First`,
//!   `Second`), [`Chosen3`], [`Chosen4`]. An alternative is passed over when it has no grant, a
//!   grant of another kind, a read-only grant where it writes, or a host that does not offer it;
//!   any other problem (a folder that failed to open) is the start's refusal. None granted is a
//!   refusal naming every alternative's fix ([`Want::NoneOf`]). On the command line its grant
//!   names the alternative: `--grant store=dir:<folder>`, `read:<folder>` (a folder read only),
//!   `net:<host>:<port>`, `env:<NAME>,..`, `whole:whole` (spec.rs `grant_arg`).
//! - [`Many<G>`]: every grant named `<param>.<member>` (`inputs.a`, `inputs.b`), each made as a
//!   `G` from its own grant, in member-name order; none is an empty `Many`, and the manifest
//!   says zero is allowed. A member's problem is the start's refusal, naming the member.
//! - A tuple `(A, B)` under one name is NOT a form: its members would have no grant names of
//!   their own, and two parameters already say the same thing, each with its own name.
//! - Scope in the type: a folder's mode already is (`ReadOnly`/`ReadWrite`), and a network
//!   endpoint's direction is ([`Connect`], [`Listen`]: a grant in the other direction is
//!   refused). A host or a path in the TYPE waits for a blaze extension (string const generics:
//!   `adt_const_params` is unstable, and every user crate would have to turn it on); meanwhile
//!   the endpoint or folder is written in the attribute's default (`#[bstd::main(api =
//!   connect("host:443"))]`), which is in the program's text and its manifest, so nothing adds a
//!   run-time lookup to fake it.
//!
//! Refused as the crate compiles (a `const` panic in the form, at the manifest's `static`, its
//! words naming the form): `Maybe<Maybe<G>>` and any form inside another (`Many<Maybe<G>>`, a
//! `Maybe` alternative); a `Maybe` of a kind that fills itself (it is always there); a `OneOf`
//! with two alternatives of one kind (nothing tells them apart), with an alternative that fills
//! itself (those after it could never be chosen), or with a `Dir<ReadWrite>` after a
//! `Dir<ReadOnly>` (any folder grant the read-write one could take, the read-only one takes
//! first; the other order, "write if granted, else read", is accepted); a `Many` of a kind that
//! fills itself.

use core::{alloc::Allocator, ops::Deref};

use blaze_data::{Int, Map, Value};
use blaze_own::{String, Vec};
use blaze_text::Writes as _;

use super::{Grant, Granting, Kind, LaunchAlloc, Manifest, Param, Want, same};
use crate::{net::Net, spec::Scope};

/// How a parameter takes its grants: its form in the manifest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// One grant, needed: every plain capability.
    One,
    /// One grant, if there is one ([`Maybe`]).
    Optional,
    /// The first of these alternatives granted, in written order ([`OneOf`]).
    OneOf(&'static [Kind]),
    /// Every grant `<param>.<member>`, zero or more ([`Many`]).
    Many,
}

/// A network endpoint's direction, where the type says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Narrow {
    /// Not narrowed by the type.
    None,
    /// Connects only ([`Connect`]).
    Connect,
    /// Listens only ([`Listen`]).
    Listen,
}

impl Form {
    /// Its name in the manifest's JSON.
    pub const fn name(self) -> &'static str {
        match self {
            Form::One => "one",
            Form::Optional => "optional",
            Form::OneOf(_) => "one_of",
            Form::Many => "many",
        }
    }
}

impl Narrow {
    /// Its name in the manifest's JSON (`"direction"`); `None` writes no key.
    pub const fn name(self) -> &'static str {
        match self {
            Narrow::None => "",
            Narrow::Connect => "connect",
            Narrow::Listen => "listen",
        }
    }
}

/// Whether two kinds are one: nothing a grant says tells them apart.
pub(crate) const fn same_kind(a: Kind, b: Kind) -> bool {
    match (a, b) {
        (Kind::Dir { write: x }, Kind::Dir { write: y }) => x == y,
        (Kind::Whole(x), Kind::Whole(y)) => same(x, y),
        _ => same(a.name(), b.name()),
    }
}

/// The form of `Maybe<G>`, `G` being of `form` and `kind`; a form that does not compose is a
/// compile error naming it.
pub const fn maybe_form(form: Form, kind: Kind) -> Form {
    match form {
        Form::One => {}
        Form::Optional => panic!("bstd::grant: `Maybe<Maybe<..>>` is refused: a grant is optional once; write `Maybe<G>`"),
        Form::OneOf(_) => panic!("bstd::grant: `Maybe<OneOf<..>>` is refused: a form inside another does not compose; take the alternatives as `OneOf` and the absence as a `Maybe` parameter of its own"),
        Form::Many => panic!("bstd::grant: `Maybe<Many<..>>` is refused: a `Many` already allows zero grants; write `Many<G>`"),
    }
    if kind.fills_itself() {
        panic!("bstd::grant: a `Maybe` of a kind that fills itself (`Out`, `In`, `Args`, `Clock`, `Mem`, `Spec`) is refused: it is always there; take it plain");
    }
    Form::Optional
}

/// The form of `Many<G>`.
pub const fn many_form(form: Form, kind: Kind) -> Form {
    if !matches!(form, Form::One) {
        panic!("bstd::grant: `Many<Maybe<..>>`, `Many<OneOf<..>>` and `Many<Many<..>>` are refused: a form inside another does not compose; write `Many<G>` of a plain capability");
    }
    if kind.fills_itself() {
        panic!("bstd::grant: a `Many` of a kind that fills itself (`Out`, `In`, `Args`, `Clock`, `Mem`, `Spec`) is refused: it takes no grant, so there is nothing to have many of");
    }
    Form::Many
}

/// The form of `OneOf<..>` over `kinds` whose forms are `forms` (same length, written order).
pub const fn one_of_form(kinds: &'static [Kind], forms: &[Form]) -> Form {
    let mut at = 0;
    while at < kinds.len() {
        if !matches!(forms[at], Form::One) {
            panic!("bstd::grant: a `OneOf` alternative that is a `Maybe`, `OneOf` or `Many` is refused: a form inside another does not compose; each alternative is a plain capability");
        }
        if kinds[at].fills_itself() {
            panic!("bstd::grant: a `OneOf` alternative that fills itself (`Out`, `In`, `Args`, `Clock`, `Mem`, `Spec`) is refused: it is always chosen, so the alternatives after it never are");
        }
        let mut before = 0;
        while before < at {
            if same_kind(kinds[before], kinds[at]) {
                panic!("bstd::grant: a `OneOf` with two alternatives of one kind is refused: no grant tells them apart");
            }
            if matches!(kinds[before], Kind::Dir { write: false }) && matches!(kinds[at], Kind::Dir { write: true }) {
                panic!("bstd::grant: a `OneOf` with `Dir<ReadWrite>` after `Dir<ReadOnly>` is refused: the read-only one takes every folder grant first, so the other is never chosen; write `OneOf<(Dir<ReadWrite>, Dir<ReadOnly>)>` (write if granted, else read)");
            }
            before += 1;
        }
        at += 1;
    }
    Form::OneOf(kinds)
}

/// Whether a problem of one alternative passes over to the next (the module's doc): it was not
/// granted for this one.
fn passes_over(want: &Want) -> bool {
    matches!(want, Want::NoGrant | Want::WrongKind { .. } | Want::Widens | Want::NotOffered(_))
}

/// A capability the program can use when it is there (the module's doc): `None` when no grant
/// of its name is given or the host offers none; a grant that is given and wrong refuses.
pub struct Maybe<G>(Option<G>);

impl<G> Maybe<G> {
    pub fn get(&self) -> Option<&G> {
        self.0.as_ref()
    }

    pub fn into_option(self) -> Option<G> {
        self.0
    }

    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }
}

impl<G: Grant> Grant for Maybe<G> {
    const KIND: Kind = G::KIND;
    const FORM: Form = maybe_form(G::FORM, G::KIND);
    const NARROW: Narrow = G::NARROW;

    /// An absent grant is never a refusal, a wrong one always is.
    fn grant(from: &Granting<'_>) -> Result<Maybe<G>, Want> {
        match G::grant(from) {
            Ok(made) => Ok(Maybe(Some(made))),
            Err(Want::NoGrant | Want::NotOffered(_)) => Ok(Maybe(None)),
            Err(want) => Err(want),
        }
    }
}

/// The alternatives of a [`OneOf`]: a tuple of two to four capabilities.
pub trait Alternatives: Sized {
    /// Which alternative was chosen, holding it.
    type Chosen;
    /// The alternatives' kinds, in written order.
    const KINDS: &'static [Kind];
    /// The alternatives' forms, in written order (each must be [`Form::One`]).
    const FORMS: &'static [Form];
    /// The first alternative granted, or why none was.
    fn choose(from: &Granting<'_>) -> Result<Self::Chosen, Want>;
}

/// The first of the alternatives `T` that is granted (the module's doc).
pub struct OneOf<T: Alternatives>(T::Chosen);

impl<T: Alternatives> OneOf<T> {
    pub fn chosen(&self) -> &T::Chosen {
        &self.0
    }

    pub fn into_chosen(self) -> T::Chosen {
        self.0
    }
}

impl<T: Alternatives> Grant for OneOf<T> {
    /// The first alternative's kind: the parameter's kind in the manifest's `kind` key; every
    /// alternative is in its `alternatives`.
    const KIND: Kind = T::KINDS[0];
    const FORM: Form = one_of_form(T::KINDS, T::FORMS);

    fn grant(from: &Granting<'_>) -> Result<OneOf<T>, Want> {
        T::choose(from).map(OneOf)
    }
}

macro_rules! alternatives {
    ($chosen:ident; $($t:ident => $variant:ident),+) => {
        /// Which alternative of a [`OneOf`] was granted, in written order.
        pub enum $chosen<$($t),+> {
            $($variant($t)),+
        }

        impl<$($t: Grant),+> Alternatives for ($($t,)+) {
            type Chosen = $chosen<$($t),+>;
            const KINDS: &'static [Kind] = &[$(<$t as Grant>::KIND),+];
            const FORMS: &'static [Form] = &[$(<$t as Grant>::FORM),+];

            fn choose(from: &Granting<'_>) -> Result<Self::Chosen, Want> {
                $(
                    match <$t as Grant>::grant(from) {
                        Ok(made) => return Ok($chosen::$variant(made)),
                        Err(want) if passes_over(&want) => {}
                        Err(want) => return Err(want),
                    }
                )+
                Err(Want::NoneOf(Self::KINDS))
            }
        }
    };
}

alternatives!(Chosen2; A => First, B => Second);
alternatives!(Chosen3; A => First, B => Second, C => Third);
alternatives!(Chosen4; A => First, B => Second, C => Third, D => Fourth);

/// The member of the grant named `grant` under the parameter `param`: `a` of `inputs.a`.
pub fn member_of<'n>(grant: &'n str, param: &str) -> Option<&'n str> {
    let rest = grant.strip_prefix(param)?.strip_prefix('.')?;
    crate::spec::is_name(rest).then_some(rest)
}

/// Whether `raw` names a grant: a parameter's name ([`crate::spec::is_name`]), or a [`Many`]'s
/// member `<param>.<member>` (each side a name, one dot).
pub const fn is_grant_name(raw: &str) -> bool {
    if crate::spec::is_name(raw) {
        return true;
    }
    let bytes = raw.as_bytes();
    let mut dot = None;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'.' {
            if dot.is_some() {
                return false;
            }
            dot = Some(at);
        }
        at += 1;
    }
    let Some(dot) = dot else { return false };
    let (param, member) = bytes.split_at(dot);
    let (Ok(param), Ok(member)) = (core::str::from_utf8(param), core::str::from_utf8(member.split_at(1).1)) else {
        return false;
    };
    crate::spec::is_name(param) && crate::spec::is_name(member)
}

/// Every grant `<param>.<member>` of the spec (the module's doc), each made as a `G`, in
/// member-name order.
pub struct Many<G> {
    members: Vec<(String<LaunchAlloc>, G), LaunchAlloc>,
}

impl<G> Many<G> {
    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Each member's name (`a` of `inputs.a`) and capability, in name order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &G)> + '_ {
        self.members.iter().map(|(name, made)| (name.as_str(), made))
    }

    /// The member named `name`.
    pub fn get(&self, name: &str) -> Option<&G> {
        self.iter().find(|(member, _)| *member == name).map(|(_, made)| made)
    }
}

impl<G: Grant> Grant for Many<G> {
    const KIND: Kind = G::KIND;
    const FORM: Form = many_form(G::FORM, G::KIND);
    const NARROW: Narrow = G::NARROW;

    fn grant(from: &Granting<'_>) -> Result<Many<G>, Want> {
        let alloc = from.alloc();
        let mut members = Vec::new_in(alloc);
        // The spec's grants are sorted by name, each name once (spec.rs `RunSpec::grants`), so
        // the members come in name order.
        for granted in from.spec().get().grants.iter() {
            let Some(member) = member_of(granted.name.as_str(), from.name) else { continue };
            let at = Granting { cx: from.cx, name: from.name, member: Some(granted.name.as_str()), given: from.given };
            match G::grant(&at) {
                // alloc: a member's name, once per member per start.
                Ok(made) => members.push((String::from_str_in(member, alloc), made)),
                Err(want) => return Err(Want::Failed(member_said(granted.name.as_str(), G::KIND, &want, alloc))),
            }
        }
        Ok(Many { members })
    }
}

/// What a member's problem says: `` `inputs.a`: granted a `net`, and it takes a `dir` ``.
fn member_said(member: &str, kind: Kind, want: &Want, alloc: LaunchAlloc) -> String<LaunchAlloc> {
    let mut said = String::new_in(alloc);
    said.str("`").str(member).str("`: ");
    match want {
        Want::NoGrant => said.str("no grant"),
        Want::WrongKind { given } => said.str("granted a `").str(given).str("`, and it takes a `").str(kind.name()).str("`"),
        Want::Widens => said.str("granted read only, and it writes"),
        Want::NotOffered(missing) => said.str("this host offers no `").str(missing.capability).str("`"),
        Want::NotBuilt(what) => said.str("its `").str(what).str("` is not built yet"),
        Want::Failed(why) => said.str(why.as_str()),
        Want::NoneOf(_) => said.str("none of its alternatives is granted"),
    };
    said
}

/// One network endpoint, connected to only: a `net` grant that listens is refused.
pub struct Connect(Net);

/// One network endpoint, listened on only: a `net` grant that connects is refused.
pub struct Listen(Net);

impl Deref for Connect {
    type Target = Net;

    fn deref(&self) -> &Net {
        &self.0
    }
}

impl Deref for Listen {
    type Target = Net;

    fn deref(&self) -> &Net {
        &self.0
    }
}

/// The `net` grant of `from`, if it goes `listen` (true) or connects (false).
fn directed(from: &Granting<'_>, listen: bool) -> Result<Net, Want> {
    let (scope, _) = from.scope("net")?;
    match scope {
        Scope::Net { listen: given, .. } if *given == listen => <Net as Grant>::grant(from),
        Scope::Net { listen: true, .. } => Err(Want::WrongKind { given: "net (listen)" }),
        Scope::Net { .. } => Err(Want::WrongKind { given: "net (connect)" }),
        other => Err(Want::WrongKind { given: other.kind_name() }),
    }
}

impl Grant for Connect {
    const KIND: Kind = Kind::Net;
    const NARROW: Narrow = Narrow::Connect;

    fn grant(from: &Granting<'_>) -> Result<Connect, Want> {
        directed(from, false).map(Connect)
    }
}

impl Grant for Listen {
    const KIND: Kind = Kind::Net;
    const NARROW: Narrow = Narrow::Listen;

    fn grant(from: &Granting<'_>) -> Result<Listen, Want> {
        directed(from, true).map(Listen)
    }
}

impl Param {
    /// The parameter `name` of type `T`, with its kind, form and direction: what the entry
    /// macros write in the manifest (`Param::of::<Maybe<Dir<ReadWrite>>>("cache")`).
    pub const fn of<T: Grant>(name: &'static str) -> Param {
        Param::new(name, T::KIND).shaped(T::FORM, T::NARROW)
    }

    /// This parameter with `form` and `narrow`: for a list made from kinds read in const
    /// (a test's slots, simtest/param.rs).
    pub const fn shaped(mut self, form: Form, narrow: Narrow) -> Param {
        self.form = form;
        self.narrow = narrow;
        self
    }
}

impl Manifest {
    /// The parameter that takes the grant named `grant`: the one of that name, or the [`Many`]
    /// whose member it is (`inputs` for `inputs.a`).
    pub fn taker(&self, grant: &str) -> Option<&Param> {
        self.param(grant).or_else(|| {
            let (param, _) = grant.split_once('.')?;
            self.param(param).filter(|param| matches!(param.form, Form::Many) && member_of(grant, param.name).is_some())
        })
    }
}

/// The keys a parameter's form and direction add to its manifest entry; nothing for a plain
/// one, so a manifest of plain parameters is byte-equal to one written before forms existed.
pub(crate) fn write_shape<A: Allocator + Clone>(param: &Param, map: &mut Map<A>, a: A) {
    let key = |k: &str| String::from_str_in(k, a.clone());
    match param.form {
        Form::One => {}
        Form::Optional => {
            map.insert(key("form"), Value::Text(key(param.form.name())));
        }
        Form::Many => {
            map.insert(key("form"), Value::Text(key(param.form.name())));
            map.insert(key("min"), Value::Int(Int::from_u64(0)));
        }
        Form::OneOf(kinds) => {
            map.insert(key("form"), Value::Text(key(param.form.name())));
            let mut alternatives = Vec::with_capacity_in(kinds.len(), a.clone());
            for kind in kinds {
                let mut alternative = Map::new_in(a.clone());
                alternative.insert(key("kind"), Value::Text(key(kind.name())));
                match kind {
                    Kind::Dir { write } => {
                        alternative.insert(key("write"), Value::Bool(*write));
                    }
                    Kind::Whole(capability) => {
                        alternative.insert(key("capability"), Value::Text(key(capability)));
                    }
                    _ => {}
                }
                alternatives.push(Value::Map(alternative));
            }
            map.insert(key("alternatives"), Value::List(alternatives));
        }
    }
    if !matches!(param.narrow, Narrow::None) {
        map.insert(key("direction"), Value::Text(key(param.narrow.name())));
    }
}

/// What names an alternative of a [`OneOf`] on the command line (`--grant store=<prefix>:..`):
/// `dir` a folder read and write, `read` a folder read only, `net`, `env`, and a whole
/// capability by its name (`fs`, `host`, ..).
pub const fn prefix(kind: Kind) -> &'static str {
    match kind {
        Kind::Dir { write: true } => "dir",
        Kind::Dir { write: false } => "read",
        Kind::Whole(capability) => capability,
        _ => kind.name(),
    }
}

/// The alternative of `kinds` that a `--grant` value `<prefix>:<rest>` names, the first of that
/// prefix, and the rest; `None` when no alternative has that prefix.
pub fn alternative_of<'v>(kinds: &[Kind], value: &'v str) -> Option<(Kind, &'v str)> {
    let (named, rest) = value.split_once(':')?;
    kinds.iter().find(|kind| prefix(**kind) == named).map(|kind| (*kind, rest))
}

/// A folder grant's path that names the world's start folder itself (`dir(".")`,
/// `--grant data=.`): `Fs::root()`, which a bstd `Path` cannot name (it has no `.` part). Every
/// other folder grant's path is a `Path` beneath it.
pub const START_FOLDER: &str = ".";

/// The scope a `--grant` value gives a parameter of `kind` narrowed by `narrow` (spec.rs
/// `grant_arg`, the one home of the grant syntax, calls it per kind): a folder with the rights
/// the parameter asks for, an endpoint (`listen:` before it listens; a [`Listen`] listens
/// without it, a [`Connect`] refuses it), variable names (sorted, each once), a whole
/// capability (`whole`).
pub fn scope_of<A: Allocator + Clone>(
    kind: Kind,
    narrow: Narrow,
    value: &str,
    alloc: A,
) -> Result<Scope<A>, crate::spec::GrantArgError> {
    use crate::spec::GrantArgError;
    let bad = GrantArgError::Value { wants: kind.scope_form() };
    match kind {
        Kind::Dir { write } => {
            if value != START_FOLDER {
                crate::path::Path::new(value).map_err(|_| bad.clone())?;
            }
            Ok(Scope::Dir { path: String::from_str_in(value, alloc), write })
        }
        Kind::Net => {
            let (listen, endpoint) = match value.strip_prefix("listen:") {
                Some(endpoint) => (true, endpoint),
                None => (matches!(narrow, Narrow::Listen), value),
            };
            if listen && matches!(narrow, Narrow::Connect) {
                return Err(bad);
            }
            crate::net::HostPort::parse(endpoint).map_err(|_| bad.clone())?;
            Ok(Scope::Net { at: String::from_str_in(endpoint, alloc), listen })
        }
        Kind::Env => {
            let mut names: Vec<String<A>, A> = Vec::new_in(alloc.clone());
            for name in value.split(',') {
                crate::env::VarName::new(name).map_err(|_| bad.clone())?;
                if !names.iter().any(|it| it == name) {
                    // alloc: a grant's variable names, once per `--grant`.
                    names.push(String::from_str_in(name, alloc.clone()));
                }
            }
            names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            Ok(Scope::Env { names })
        }
        Kind::Whole(capability) if value == "whole" => Ok(Scope::Whole { capability: String::from_str_in(capability, alloc), device: None }),
        Kind::Whole(capability) if super::narrows_to_device(capability) => {
            let device = value.strip_prefix("device:").ok_or(bad.clone())?;
            if crate::name::refused(device).is_some() {
                return Err(bad);
            }
            // alloc: a grant's capability and device names, once per `--grant`.
            let device = Some(String::from_str_in(device, alloc.clone()));
            Ok(Scope::Whole { capability: String::from_str_in(capability, alloc), device })
        }
        kind if kind.fills_itself() => Err(GrantArgError::FillsItself),
        _ => Err(bad),
    }
}

/// The fix line of a [`OneOf`] that none of whose alternatives is granted: each alternative's
/// `--grant` line, `or` between.
pub(crate) fn none_of_fix(name: &str, kinds: &[Kind], out: &mut String<LaunchAlloc>) {
    out.str("none of its alternatives is granted; add ");
    for (at, kind) in kinds.iter().enumerate() {
        if at > 0 {
            out.str(" or ");
        }
        out.str("`--grant ").str(name).str("=").str(prefix(*kind)).str(":").str(kind.scope_form()).str("`");
    }
}

#[cfg(test)]
mod tests;
