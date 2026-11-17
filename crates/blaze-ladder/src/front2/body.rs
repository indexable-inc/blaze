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
use blaze_lower::scoped::Prepares;
use blaze_lower::typeck::{ArgAt, ConstGapWhy, ConstTerm, Refused, TypeckBody, Types, to_typeck};
use blaze_lower::{Lowered, View};
use blaze_own::Alloc;
use blaze_surface::Range;
use blaze_typeck::{ExprId, ItemKey, Name, OwnTys, PTy, PortableTys};

use super::check::{BodyScope, MOST_ROUNDS};
use super::heads::{CrateCfg, FnHead, HeadError, Maps, Store, Want, own_generics};
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::scoped::ScopedBy;
use super::source::NamesSource;
use super::spell::Spelling;
use super::type_miss::TypeMiss;
use super::types::{Bounds, Builder, Env, ParamBound, SelfTy, Why};
use crate::resolve::Paths;

/// What a body's lowering did not take, by its range in the item's text (or past it, in an
/// expansion appended to it).
#[derive(Debug)]
pub enum BodyFinding {
    /// A macro call whose path names no macro the expander could run. Its path and why are
    /// rendered: the expander's finding borrows its frame, which ends with the lowering.
    /// `cause` is the typed why's variant chain (`Failed(Expand(Host`), what a census counts by.
    Macro { at: Range, path: Box<str, Global>, why: Box<str, Global>, cause: Box<str, Global> },
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
    /// Its lowering's record is malformed (a row, a range), or its text's marks are lost: `item`
    /// is the item whose text it is (a member's container, an anonymous constant's module),
    /// `owner` the checker key of the body asked (the member itself), so both are named.
    Record { item: LibKey, owner: ItemKey, why: Why },
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
    /// An identifier pattern's name, asked at the fn's module ([`ModuleValues`]), names an item
    /// no outline holds.
    PatternName { item: LibKey, error: crate::resolve::ResolveError },
    /// The traits in scope at its module could not be read.
    Traits { item: LibKey, error: blaze_scope::names::PathError },
    /// A member fn whose head the store has not made (its container's head is made first).
    NoHead(ItemKey),
    /// A member fn with no body (a required trait method): nothing to check.
    NoBody(ItemKey),
    /// The constant `item` an array length of the body (or of a head it needs) names has no
    /// value: the evaluator's refusal.
    Const { item: LibKey, error: Box<super::consts::ConstFail, Global> },
    /// [`super::lower_made::lower_made`] made `need` and the lowering asked for it again at
    /// once (no progress), or met [`super::lower_made::MOST_MADE`] makes: `after` is how many
    /// heads and constants it had made for `item`.
    Made { item: LibKey, need: super::lower_made::Needed, after: u32 },
}

impl core::fmt::Display for BodyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BodyError::Const { item, error } => write!(f, "constant {item:x}, an array length: {error}"),
            BodyError::NoText(k) => write!(f, "fn {k:x}: the names library holds no text for it (a member of a trait or an impl is lowered by `lower_member`)"),
            BodyError::NoHead(k) => write!(f, "member fn {k:?}: its head is not in the store, and no want makes it (it is no fn or constant of a trait or impl whose head front2 made)"),
            BodyError::NoBody(k) => write!(f, "fn {k:?}: its text has no body (a required trait method, an intrinsic or a foreign fn)"),
            BodyError::Cfg(k) => write!(f, "fn {k:x}: its lowering asks for a `cfg` the crate does not answer"),
            BodyError::Generics { item, why } => write!(f, "fn {item:x}: its generic parameters: {why:?}"),
            BodyError::Record { item, owner, why } => write!(f, "body {owner:?} (in the text of {item:x}): its lowered record: {why:?}"),
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
            BodyError::Made { item, need, after } => write!(f, "fn {item:x}: still needs {need:?} after {after} makes (at most {})", super::lower_made::MOST_MADE),
            BodyError::Traits { item, error } => write!(f, "fn {item:x}: the traits in scope at its module: {error}"),
            BodyError::Table { item, error } => write!(f, "fn {item:x}: its own type table: {error:?}"),
            BodyError::PatternName { item, error } => write!(f, "fn {item:x}: an identifier pattern's name at its module: {error:?}"),
        }
    }
}

/// Why [`ModuleValues`] could not answer.
enum ValuesFail {
    Resolve(crate::resolve::ResolveError),
    Head(HeadError),
}

/// The module's answer for an identifier pattern of a body lowered in `module`
/// ([`blaze_lower::ValueNames`]): the name resolved as a one-segment value path written there,
/// through the same [`Resolver::path`] a body's paths take (its `use`s, globs and the
/// prelude), and a constructor's shape from its ADT's head in the store. The first failure is
/// kept and the lowering's `Need` turned into it.
struct ModuleValues<'n, 's, S> {
    names: S,
    module: LibKey,
    store: &'s mut Store<'n>,
    keys: &'s mut Keys,
    failed: Option<ValuesFail>,
    /// The body: its crate, its item (a member's: its container) and the text it is lowered
    /// from; and its blocks' modules, scanned at the first question a block's `use` raises
    /// ([`super::glob_uses::by_uses`]).
    krate: usize,
    item: LibKey,
    text: &'n str,
    blocks: Option<super::glob_uses::PatternBlocks<'static>>,
}

impl<'n, S: NamesSource<'n>> ModuleValues<'n, '_, S> {
    /// The shape of `adt`'s variant `index` (0 for a struct), its head made when not yet.
    fn shape(&mut self, adt: LibKey, index: u32, unit: blaze_lower::ValueName) -> Result<blaze_lower::ValueName, ValuesFail> {
        use blaze_lower::ValueName as V;
        use blaze_typeck::CtorKind;
        let k = checker_of(adt);
        if !self.store.adts.contains_key(&k) {
            self.store.ensure(self.names, self.keys, Want::Adt(adt)).map_err(ValuesFail::Head)?;
        }
        let ctor = self.store.adts.get(&k).and_then(|a| a.variants.get(index as usize)).map(|v| v.ctor);
        Ok(match ctor {
            Some(CtorKind::Unit) => unit,
            Some(CtorKind::Tuple) => V::TupleCtor,
            Some(CtorKind::Braces) | None => V::Nothing,
        })
    }
}

impl<'n, S: NamesSource<'n>> blaze_lower::ValueNames for ModuleValues<'n, '_, S> {
    /// The name through the `use` items of the blocks around the pattern: a named import's
    /// path, or a glob import's (`use E::*;` makes `A` the variant `E::A`), read by
    /// [`super::glob_uses::by_uses`], where the rules are quoted; its shape as
    /// [`Self::value_name`] gives a module's. A refusal there is the body's, by its name.
    fn value_name_by_uses(&mut self, view: &blaze_lower::View<'_>, name: &str, uses: &[blaze_lower::block_uses::BlockUse<'_>], locals: &[&str]) -> Option<blaze_lower::ValueName> {
        use crate::resolve::Kind;
        use blaze_lower::ValueName as V;
        if self.failed.is_some() {
            return Some(V::Need);
        }
        let (names, krate, item, text) = (self.names, self.krate, self.item, self.text);
        let planted = self.store.locals.planted();
        let alloc = self.store.tys.alloc();
        let blocks = self.blocks.get_or_insert_with(|| super::glob_uses::PatternBlocks::of(names, krate, item, text, alloc));
        let target = match super::glob_uses::by_uses(&Paths { names }, self.module, item, blocks, view, name, uses, locals, planted, alloc) {
            Ok(Some(target)) => target,
            Ok(None) => return None,
            Err(gap) => {
                self.failed = Some(ValuesFail::Head(HeadError::Item(gap.item, Why::Unsupported(gap.form, gap.range))));
                return Some(V::Need);
            }
        };
        let answer = match target.kind {
            Kind::Const => Ok(V::Const),
            Kind::Static => Ok(V::Static),
            Kind::Struct if super::source::NamesSource::is_ctor(self.names, target.key) => self.shape(target.key, 0, V::UnitStruct),
            Kind::Variant { adt, index } => self.shape(adt, index, V::UnitVariant),
            _ => Ok(V::Nothing),
        };
        Some(answer.unwrap_or_else(|fail| {
            self.failed = Some(fail);
            V::Need
        }))
    }

    fn value_name(&mut self, name: &str) -> blaze_lower::ValueName {
        use crate::resolve::{Kind, Ns, ResolveError};
        use blaze_lower::ValueName as V;
        if self.failed.is_some() {
            return V::Need;
        }
        let target = match (Paths { names: self.names }).path(self.module, false, &[name], Ns::Value) {
            Ok(t) => t,
            // Nothing by that name: the pattern binds it.
            Err(ResolveError::Path(_)) => return V::Nothing,
            Err(e @ ResolveError::NoText { .. }) => {
                self.failed = Some(ValuesFail::Resolve(e));
                return V::Need;
            }
        };
        let answer = match target.kind {
            Kind::Const => Ok(V::Const),
            Kind::Static => Ok(V::Static),
            Kind::Struct if super::source::NamesSource::is_ctor(self.names, target.key) => self.shape(target.key, 0, V::UnitStruct),
            Kind::Variant { adt, index } => self.shape(adt, index, V::UnitVariant),
            _ => Ok(V::Nothing),
        };
        answer.unwrap_or_else(|fail| {
            self.failed = Some(fail);
            V::Need
        })
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
    /// The body's own types (closures, futures, holes, written types the records' table
    /// lacked), numbered after the records' table (`Store::tys`) as it stood when the body was
    /// lowered: read with it, `tys.view(&store.tys)`. A record is answered from `Store::tys`
    /// alone, which never holds a body's type, whatever bodies were checked before.
    pub tys: OwnTys<'static>,
    pub params: Vec<(Name, u32), Global>,
    pub self_: SelfTy,
    /// The traits in scope at `module`, checker keys, sorted.
    pub traits: Vec<ItemKey, Global>,
    /// The traits in scope at each block whose list is not `traits`: the blocks' `use`s
    /// ([`super::block_traits`]).
    pub blocks: super::block_traits::BlockTraits,
    /// Each written type that names nothing (E0412, [`TypeMiss`]): converted as the error type,
    /// reported by the check beside its own findings (front2 check.rs `check_rounds_as`).
    pub type_misses: Vec<TypeMiss, Global>,
}

impl<'n> Body<'n> {
    /// The scope the check loop resolves the body's paths in; `spell` is
    /// `Spelling::of(&self.lowered.view())`.
    pub fn scope<'s>(&'s self, spell: &'s Spelling<'s>) -> BodyScope<'s> {
        BodyScope {
            owner: self.owner,
            module: self.module,
            params: &self.params,
            self_: &self.self_,
            spell,
            traits: &self.traits,
            blocks: &self.blocks,
            path_marks: &self.typeck.path_marks,
        }
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
struct BodyTypes<'b, S> {
    builder: Builder<'b, Maps, S>,
    env: Env<'b>,
    failed: Option<Why>,
    /// The item's text, which a written type's range is relative to: a [`TypeMiss`] reads its
    /// path there.
    text: &'b str,
    /// Each written type that names nothing (E0412), converted as the error type so the rest of
    /// the body is checked.
    misses: Vec<TypeMiss, Global>,
    /// The body's blocks that are modules of the names library holding a struct, enum, union
    /// or type alias ([`super::locals::BodyLocals::blocks`]); empty for most bodies.
    blocks: &'b [super::locals::BlockModule],
    /// The items declared in bodies, and whose body this is: a path through a type alias is
    /// expanded only where no item of the body takes its first segment
    /// ([`super::world_paths::AliasPaths`]).
    locals: &'b super::locals::Locals<'b>,
    owner: ItemKey,
    /// An `async fn`'s written return type (its id in the body's view) and the type its head
    /// made of it in return position ([`super::async_output`]): the coroutine's output
    /// `to_typeck` reads, never lowered again in the body's env (which has no return opaques).
    async_ret: Option<(lf::TypeId, PTy)>,
}

impl<'b, 'n, S: NamesSource<'n>> BodyTypes<'b, S> {
    /// Where the type `t` is written: the body's names, read at the module of the innermost
    /// block around it that declares a type (the Reference, "Statements", "Item
    /// declarations": "Declaring an item within a statement block restricts its scope to the
    /// block containing the statement"; the block's module is its items plus a glob import of
    /// the scope outside, so every other name reads as before). The body's generic
    /// parameters and `Self` stay in scope: they are the body's, not the block's items'.
    /// A type an expansion wrote reads at the block of the outermost macro call that made it;
    /// one written with `super` names nothing of a block (a block's item has no path) and
    /// reads at the body's module, where `super` is the module's parent.
    fn env_at(&self, view: &View<'_>, t: lf::TypeId) -> Result<Env<'b>, Refused> {
        let mut env = self.env;
        if self.blocks.is_empty() {
            return Ok(env);
        }
        let range = view.type_range(t)?;
        // A type an expansion wrote is read at the outermost macro call that made it (the
        // Reference, "Macros By Example", "Hygiene": macro_rules! "local variables are looked up
        // at the macro definition site while other symbols are looked up at the macro
        // invocation site"; builtin `offset_of!` takes its type from the call's tokens): the
        // call's range ([`View::site`]) places it in its block.
        let r = match view.item_relative(range) {
            Some(r) => r,
            None => view.site(range)?,
        };
        if view.text_at(range)?.contains("super") {
            return Ok(env);
        }
        if let Some(b) = self.blocks.iter().rev().find(|b| b.open <= r.start && r.start < b.close) {
            env.module = b.module;
        }
        Ok(env)
    }
}

impl<'n, S: NamesSource<'n>> Types for BodyTypes<'_, S> {
    fn ty(&mut self, view: &View<'_>, t: lf::TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        if let Some((at, out)) = self.async_ret {
            if at == t {
                return Ok(out);
            }
        }
        let env = self.env_at(view, t)?;
        match self.builder.ty(tys, &env, view, t) {
            Ok(ty) => Ok(ty),
            Err(why) => {
                // A type the body's module does not resolve, in a body with a glob import in
                // a block: the name may be the glob's to give, which the type builder (it
                // reads at one module) does not ask. Refused by that name, never as a
                // program that names nothing (super::glob_uses, "Owed").
                let why = match why {
                    Why::Resolve(_, r) if super::glob_uses::any_glob(self.locals, self.owner) => Why::Unsupported(super::glob_uses::TYPE_THROUGH_GLOB, r),
                    why => why,
                };
                // A path the names library decided names nothing: the program's E0412, the type
                // the error type, the body checked for everything else (type_miss.rs).
                if let Why::Resolve(crate::resolve::ResolveError::Path(e), r) = &why
                    && let Some(miss) = TypeMiss::of(self.builder.resolver.names, self.text, *e, *r)
                {
                    self.misses.push(miss);
                    return tys.intern(blaze_typeck::PNode::Error).map_err(Refused::from);
                }
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
        self.const_arg_in(view, c, tys, None, None)
    }

    /// [`Types::const_term`], typed by the parameter the argument fills when the path's item
    /// is known ([`BodyTypes::arg_param`]): the Reference, "Constant evaluation", "Const
    /// context": a const generic argument is a const context, so `enc_16lsd::<{
    /// U128_MAX_DEC_N - 16 }>(..)` (the pinned core, library/core/src/fmt/num.rs, `impl
    /// u128`'s `_fmt_inner`) is an anonymous constant of `OFFSET`'s type `usize`, evaluated
    /// once, where the checker's hole took any value. A parameter whose type is not known
    /// here (a method's, an associated fn's, a path a block of the body may name) is the
    /// checker's, as before.
    fn const_term_at(&mut self, view: &View<'_>, c: lf::ConstIdx, at: ArgAt, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let range = view.site(view.const_range(c)?)?;
        let made = match self.arg_param(view, at) {
            Ok(want) => {
                let scope = view.path(at.path)?.scope;
                self.const_arg_in(view, c, tys, want, scope)
            }
            Err(why) => {
                self.failed = Some(why);
                Err(Refused::Form { what: "a const argument", range })
            }
        };
        self.arg_term(made)
    }

    /// [`Types::const_arg`], with what blaze cannot value yet a GAP ([`const_gap`]): the body
    /// is converted with a hole there and goes on to its check, which reports the gap
    /// (check.rs, `Gap::BodyConst`). Every other refusal stays one (`Why::Need`,
    /// `Why::NeedConst`: the driver makes the record and converts again; a program's error).
    fn const_term(&mut self, view: &View<'_>, c: lf::ConstIdx, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let made = self.const_arg(view, c, tys);
        self.arg_term(made)
    }

    /// `[x; len]`'s length as a const term, by the one home for written constants
    /// ([`Builder::const_term`], at `usize`: the Reference, "Array expressions"): `_` a hole,
    /// the fn's own const parameter its `ConstParam` (`[0u8; N]`), a literal or a constant's
    /// value a `ConstValue` ([`Why::NeedConst`] until the driver has evaluated it), an
    /// expression naming an item of the body a constant of the body ([`BodyTypes::body_anon`]),
    /// and a gap for an expression over a generic parameter (`[0u8; N * 2]`).
    fn repeat_term(&mut self, view: &View<'_>, len: ExprId, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let at = view.expr_range(len)?;
        let range = view.site(at)?;
        let text = view.text_at(at)?.trim();
        let usize_node = blaze_typeck::PNode::Uint(blaze_typeck::UintTy::Usize);
        // A `const` item declared in the body comes first (`const MOST: usize = ..; [x; MOST]`:
        // front2/const_open.rs `local_const`): a block's item shadows the module's name.
        let made = match super::const_open::local_const(self.locals, self.owner, self.builder.heads, view, len, text, range) {
            Ok(Some(bits)) => match super::types::intern(tys, usize_node) {
                Ok(ty) => super::types::intern(tys, blaze_typeck::PNode::ConstValue { ty, bits }),
                Err(why) => Err(why),
            },
            Ok(None) => match self.body_anon(text, scope_within(view, at)?, usize_node, range, tys) {
                Some(made) => made,
                None => self.builder.const_term(tys, &self.env, text, range, &|tys| super::types::intern(tys, usize_node)),
            },
            Err(why) => Err(why),
        };
        self.term(made, "an array repeat length", range)
    }

    /// A generic argument of a body path written as one plain name (`foo::<N>()`,
    /// `W::<SIZE>`). The Reference, "Generic parameters", "Const generics": "When there is
    /// ambiguity if a generic argument could be resolved as either a type or const argument,
    /// it is always resolved as a type." So, as a written type's argument is decided
    /// ([`Builder::path_args`], the same rule): a type parameter, or a name the names library
    /// answers as a type, is a type ([`Types::ty`]); a const parameter of the fn is its
    /// `ConstParam`; a name that resolves as a value is a constant ([`Builder::const_term`],
    /// its type left to the checker, which takes the parameter's). A name that resolves as
    /// neither is read as a type, whose refusal names it. An item the body declares takes
    /// part in the same order (const_open.rs `local_arg`): a type in scope first (a block's
    /// type, then a type parameter or the module's type), and only a name that is no type is
    /// tried as a `const` of the body, before the fn's const parameters and the module's
    /// values, which a block's item shadows.
    fn name_arg(&mut self, view: &View<'_>, t: lf::TypeId, name: &str, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        use crate::resolve::{Kind, Ns};
        use super::const_open::LocalArg;
        let range = view.site(view.type_range(t)?)?;
        // Its refusal (a local constant not evaluated yet, a local value that is no constant)
        // stands only once the name is known to be no type.
        let local = super::const_open::local_arg(self.locals, self.owner, self.builder.heads, view, t, name, range);
        if matches!(local, Ok(Some(LocalArg::Type))) {
            return Types::ty(self, view, t, tys).map(ConstTerm::Is);
        }
        let param = self.env.params.iter().find(|(n, _)| *n == Name::of(name)).map(|&(_, i)| i);
        let is_const = param.is_some_and(|i| self.env.consts.iter().any(|(c, _)| *c == i));
        let a_type = !is_const
            && (param.is_some()
                || match self.builder.resolver.path(self.env.module, false, &[name], Ns::Type) {
                    Ok(f) => f.consumed == 1 && matches!(f.kind, Kind::Struct | Kind::Enum | Kind::Union | Kind::TypeAlias | Kind::Builtin(_) | Kind::WideInt { .. }),
                    Err(_) => false,
                });
        if a_type {
            return Types::ty(self, view, t, tys).map(ConstTerm::Is);
        }
        match local {
            // A local `const` is that constant's value, its type left to the checker as for a
            // module's constant.
            Ok(Some(LocalArg::Const(bits))) => {
                let made = match super::types::intern(tys, blaze_typeck::PNode::Infer) {
                    Ok(ty) => super::types::intern(tys, blaze_typeck::PNode::ConstValue { ty, bits }),
                    Err(why) => Err(why),
                };
                return self.term(made, "a const argument", range);
            }
            Err(why) => return self.term(Err(why), "a const argument", range),
            Ok(Some(LocalArg::Type) | None) => {}
        }
        if !is_const && self.builder.resolver.path(self.env.module, false, &[name], Ns::Value).is_err() {
            return Types::ty(self, view, t, tys).map(ConstTerm::Is);
        }
        let made = self.builder.const_term(tys, &self.env, name, range, &|tys| super::types::intern(tys, blaze_typeck::PNode::Infer));
        self.term(made, "a const argument", range)
    }

    /// A path through a type alias whose aliased type fixes arguments (`type Call<P, C> =
    /// Raw<P, C, u8>;`, `Call::new(..)`): the aliased type's arguments for the alias's
    /// segment ([`super::world_paths::AliasPaths::expand`], where the rule is quoted). An
    /// alias whose head is not made yet is `Why::Need`: the body is converted again once it
    /// is, as for a written type.
    fn alias_args(&mut self, view: &View<'_>, path: blaze_typeck::PathIdx, written: &[&[blaze_typeck::PTy]], tys: &mut PortableTys<'_>, out: &mut dyn FnMut(blaze_typeck::PTy)) -> Result<Option<u32>, Refused> {
        let paths = super::world_paths::AliasPaths { builder: &self.builder, env: &self.env, locals: self.locals, owner: self.owner };
        match paths.expand(view, path, written, tys, out) {
            Ok(at) => Ok(at),
            Err(why) => {
                self.failed = Some(why);
                Err(Refused::Form { what: "a path through a type alias", range: view.site(view.path_range(path)?)? })
            }
        }
    }

    /// The check reports every const gap of a body (check.rs `check_body_counted`:
    /// `Gap::BodyConst`), so a body with one is converted and checked, never passed clean.
    fn takes_const_gaps(&self) -> bool {
        true
    }
}

/// The gap a refused const term of a BODY is, when blaze cannot value it yet (the body goes
/// on with a hole): an expression over a generic parameter (types.rs `const_value_bits`
/// refuses it by these words: blaze-typeck's type form has no node for an unevaluated
/// expression), or an anonymous constant whose type is not known where the body is converted
/// or that the evaluator refused ([`Why::ConstPath`]). `None`: a refusal that stays one.
fn const_gap(why: &Why) -> Option<ConstGapWhy> {
    match why {
        Why::Unsupported(what, _) if *what == "a constant expression over a generic parameter" => Some(ConstGapWhy::OverGeneric),
        Why::ConstPath(text, _) => Some(ConstGapWhy::AnonInBody(blaze_lower::typeck::GapText::of(text))),
        _ => None,
    }
}

impl<'n, S: NamesSource<'n>> BodyTypes<'_, S> {
    /// A made const term: the term, its gap ([`const_gap`]), or the refusal `what` at `range`
    /// with the builder's reason kept.
    fn term(&mut self, made: Result<PTy, Why>, what: &'static str, range: Range) -> Result<ConstTerm, Refused> {
        match made {
            Ok(t) => Ok(ConstTerm::Is(t)),
            Err(why) => match const_gap(&why) {
                Some(gap) => Ok(ConstTerm::Gap(gap)),
                None => {
                    self.failed = Some(why);
                    Err(Refused::Form { what, range })
                }
            },
        }
    }

    /// A made const argument as a term: the term, its gap when the builder's reason is one
    /// ([`const_gap`]: the reason is dropped, the body goes on with a hole), or the refusal.
    fn arg_term(&mut self, made: Result<PTy, Refused>) -> Result<ConstTerm, Refused> {
        match made {
            Ok(t) => Ok(ConstTerm::Is(t)),
            Err(refused) => match self.failed.as_ref().and_then(const_gap) {
                Some(gap) => {
                    self.failed = None;
                    Ok(ConstTerm::Gap(gap))
                }
                None => Err(refused),
            },
        }
    }

    /// [`Types::const_arg`] of the argument `c`: its parameter's type `want` when known (else
    /// a hole the checker fills), and `scope`, the block its path is written in when known
    /// (an expression naming an item of the body is then a constant of the body,
    /// [`BodyTypes::body_anon`]).
    fn const_arg_in(&mut self, view: &View<'_>, c: lf::ConstIdx, tys: &mut PortableTys<'_>, want: Option<PTy>, scope: Option<lf::ScopeId>) -> Result<PTy, Refused> {
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
        let want_ty = |tys: &mut PortableTys<'_>| match want {
            Some(t) => Ok(t),
            None => super::types::intern(tys, blaze_typeck::PNode::Infer),
        };
        let made = match variant {
            Some((adt, index)) => tys.variant_value(checker_of(adt), index).map_err(|e| Why::Program(format!("the type table refused a variant's value: {e:?}").into_boxed_str(), range)),
            None => {
                let node = match want {
                    Some(t) => tys.view().get(t).copied(),
                    None => None,
                };
                // Of a type the checker finds (`Infer`): an expression naming an item of the
                // body is still the body's constant, never read at the module, where the name
                // means another item; it is filed with its type left to the checker when that
                // type is its own (anon.rs `written_ty`: std's `Ipv4Addr` `Display`,
                // `DisplayBuffer::<{ LONGEST_IPV4_ADDR.len() }>::new()`), else refused by its
                // text.
                let local = self.body_anon(text, scope, node.unwrap_or(blaze_typeck::PNode::Infer), range, tys);
                match local {
                    Some(made) => made,
                    None => self.builder.const_term(tys, &self.env, text, range, &want_ty),
                }
            }
        };
        match made {
            Ok(t) => Ok(t),
            Err(why) => {
                self.failed = Some(why);
                Err(Refused::Form { what: "a const argument", range })
            }
        }
    }

    /// Whether the constant expression `text`, written in block `scope` of this body, names an
    /// item declared in the body ([`super::locals::Locals::find`] in either namespace, from
    /// that block outwards). Each identifier is asked (a number's digits and suffix are one
    /// token, never an identifier); one that only a block's glob `use` gives is not seen.
    fn names_body_item(&self, text: &str, scope: u32) -> bool {
        use super::locals::LocalNs;
        let bytes = text.as_bytes();
        let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
        let mut i = 0;
        while i < bytes.len() {
            if !word(bytes[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && word(bytes[i]) {
                i += 1;
            }
            if bytes[start].is_ascii_digit() {
                continue;
            }
            // `word` splits only at ASCII bytes, so both ends are char boundaries.
            let Some(ident) = text.get(start..i) else { continue };
            if self.locals.find(self.owner, Some(scope), ident, LocalNs::Value).is_some() || self.locals.find(self.owner, Some(scope), ident, LocalNs::Type).is_some() {
                return true;
            }
        }
        false
    }

    /// The constant expression `text` of type `ty` written in block `scope` of this body, when
    /// it names an item declared in the body: an anonymous constant of the body
    /// ([`super::anon::Ask::in_body`]), its names read in that block first, evaluated once by
    /// the same machine as any constant ([`super::anon::body_value`]). An expression that also
    /// names a generic parameter in scope has no value here (the OverGeneric gap, as for the
    /// module's). `None`: the expression names no item of the body (or no block is known),
    /// and the module's reading applies.
    fn body_anon(&self, text: &str, scope: Option<lf::ScopeId>, ty: blaze_typeck::PNode, range: Range, tys: &mut PortableTys<'_>) -> Option<Result<PTy, Why>> {
        let mut text = text.trim();
        if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            text = inner.trim();
        }
        let scope = scope?;
        if !self.names_body_item(text, scope.0) {
            return None;
        }
        // `Self` where it is a ground type (`Self::BITS` in `impl DecodeValue for u128`) is no
        // parameter: the constant is filed with it in scope. A generic parameter, or a `Self`
        // that is not ground, leaves the expression with no value here.
        let self_ = super::const_open::ground_self(tys.view(), &self.env, text);
        if self_.is_none() && super::anon::mentions(text, self.env.params) {
            return Some(Err(Why::Unsupported("a constant expression over a generic parameter", range)));
        }
        let Some(ask) = super::anon::Ask::in_body(self.env.module, ty, text, self.owner, scope.0, self_) else {
            // A type with no written name (not `bool`, `char` or an integer): no item text
            // can be made of it, refused by the expression's name.
            return Some(Err(Why::ConstPath(Box::from(text), range)));
        };
        let bits = match super::anon::body_value(self.builder.heads, ask, range) {
            Ok(bits) => bits,
            Err(why) => return Some(Err(why)),
        };
        Some(super::types::intern(tys, ty).and_then(|t| super::types::intern(tys, blaze_typeck::PNode::ConstValue { ty: t, bits })))
    }

    /// The declared type of the const parameter that the argument at `at` fills, when the
    /// path's segment names an item this body's module resolves (the Reference, "Generic
    /// parameters", "Const generics": an argument's type is its parameter's): a free fn's, an
    /// ADT's, a trait's (its arguments follow `Self`) or a type alias's const parameters, as
    /// its head filed them ([`super::types::Heads::const_params`]). A head not made yet is
    /// [`Why::Need`] (the driver makes it and converts again, `lower_at`). `None`: the type is
    /// the checker's to find: a segment after an anchor or a generic parameter, an associated
    /// item (`Ty::f::<{..}>`), or a path whose first segment a block of the body may name (a
    /// local item, a named or glob import), which the module's names do not see.
    fn arg_param(&self, view: &View<'_>, at: ArgAt) -> Result<Option<PTy>, Why> {
        use crate::resolve::{Kind, Ns};
        // A path the type builder does not read as written (`$crate`, parenthesized
        // arguments) is the checker's.
        let Ok((abs, texts, segs, _)) = super::types::path_texts_of(view, at.path, false) else { return Ok(None) };
        let seg = at.segment as usize;
        if seg >= texts.len() || segs.iter().take(seg + 1).any(|s| !matches!(s.kind, lf::SegKind::Ident | lf::SegKind::Crate | lf::SegKind::SelfMod | lf::SegKind::Super)) {
            return Ok(None);
        }
        if self.env.params.iter().any(|(n, _)| *n == Name::of(texts[0])) {
            return Ok(None);
        }
        // A body that is a local item's or a body constant's, or whose blocks declare
        // something the first segment may be: the module's answer may not be the path's.
        if self.locals.head(self.owner).is_some() {
            return Ok(None);
        }
        if let Some(b) = self.locals.body(self.owner) {
            if b.rows.iter().any(|r| r.name == texts[0] || r.name == super::glob_uses::GLOB) {
                return Ok(None);
            }
        }
        let upto = &texts[..=seg];
        let ns = if seg + 1 == texts.len() { Ns::Value } else { Ns::Type };
        let Ok(found) = self.builder.resolver.path(self.env.module, abs, upto, ns) else { return Ok(None) };
        if found.consumed as usize != upto.len() {
            return Ok(None);
        }
        let index = match found.kind {
            Kind::Trait => at.index + 1,
            Kind::Fn | Kind::Struct | Kind::Enum | Kind::Union | Kind::TypeAlias => at.index,
            _ => return Ok(None),
        };
        let Some(consts) = super::types::Heads::const_params(self.builder.heads, found.key) else { return Err(Why::Need(found.key)) };
        // No const parameter at that place: the checker's to report (E0747), with its own
        // reading of the arguments.
        Ok(consts.iter().find(|(i, _)| *i == index).map(|&(_, t)| t))
    }
}

/// The block of the first path written inside `at` (an expression's range), which is the
/// block the expression's names are read in; `None` when no path is written there (the
/// expression names nothing).
fn scope_within(view: &View<'_>, at: blaze_lower::BodyRange) -> Result<Option<lf::ScopeId>, Refused> {
    for p in 0..view.paths.len() {
        let p = blaze_typeck::PathIdx(p as u32);
        let r = view.path_range(p)?;
        if at.cover(r) == Some(at) {
            if let Some(scope) = view.path(p)?.scope {
                return Ok(Some(scope));
            }
        }
    }
    Ok(None)
}

/// Lowers and converts the body of the free fn `item`, making the heads its written types need.
/// `prepared` is the build's one set of prepared macro definitions (each kept by its item's
/// key), shared by every body the build lowers.
pub fn lower_fn<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, item: LibKey) -> Result<Body<'n>, BodyError> {
    lower_fn_into(names, prepared, keys, store, item, None)
}

/// Lowers `body`'s fn again, its types into `into` (a copy of `store.tys` made after every
/// head it needs: a head it would still need is a refusal), for a driver that reads every
/// body and record from one table (the run path's seam). The returned body's own `tys` is
/// empty: its types are `into`'s.
pub fn lower_again<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, body: &Body<'n>, into: &mut PortableTys<'static>) -> Result<Body<'n>, BodyError> {
    match keys.library_in(names, body.owner) {
        Some(lib) => lower_fn_into(names, prepared, keys, store, lib, Some(into)),
        None => lower_member_into(names, prepared, keys, store, body.owner, Some(into)),
    }
}

fn lower_fn_into<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, item: LibKey, into: Option<&mut PortableTys<'static>>) -> Result<Body<'n>, BodyError> {
    let krate = names.crate_of(item).ok_or(BodyError::NoText(item))?;
    // A fn declared in a body: its kept text, its body lowered in its enclosing body's module,
    // its generics read at its block's module as its head is (`LocalHead::names_at`).
    let (module, head_at, text) = match store.locals.head(checker_of(item)) {
        Some(h) => (h.module, h.names_at, h.text),
        None => names.text_of(item).map(|it| (it.module, it.module, it.text)).ok_or(BodyError::NoText(item))?,
    };
    // A crate whose cfg or edition the source has not read has no text for the item either.
    let atoms = names.cfg_atoms(krate).ok_or(BodyError::NoText(item))?;
    let edition = names.edition_of(krate).ok_or(BodyError::NoText(item))?;
    let sig = blaze_lower::lower_signature(text, edition, &mut CrateCfg(atoms), store.tys.alloc()).map_err(|_| BodyError::Cfg(item))?;
    let mut params = Vec::new_in(Global);
    let mut consts = Vec::new_in(Global);
    // Its generics resolve by their tokens' marks, as its head's (front2 item_site.rs); a local
    // fn's are its enclosing body's when a macro wrote that body (locals.rs `LocalHead::marks`),
    // as `Store::marks` reads its head.
    let marks = match store.locals.head(checker_of(item)) {
        Some(h) => h.marks,
        None => super::item_site::item_marks(names, item, text).map_err(|e| BodyError::Generics { item, why: e.why() })?,
    };
    let b = Builder { resolver: Paths { names }, heads: &store.maps, marks };
    own_generics(&b, &mut store.tys, head_at, &sig.view(), 0, &mut params, &mut consts).map_err(|why| BodyError::Generics { item, why })?;
    let mut bounds = Vec::new_in(Global);
    let env = Env { module: head_at, params: &params, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &consts };
    b.param_bounds(&mut store.tys, &env, &sig.view(), 0, &mut bounds).map_err(|why| BodyError::Generics { item, why })?;
    // Its body's words keep the same marks as its generics: a fn a macro wrote is lowered with
    // them (`Site::marks`).
    let site = Site { item, owner: checker_of(item), krate, module, text, marks, params, consts, bounds, self_: SelfTy::None, self_assoc: Vec::new_in(Global) };
    lower_at(names, prepared, keys, store, site, into)
}

/// Lowers and converts the body of the member fn `key` of a trait or an impl, in the env its
/// head was made in ([`FnHead`]: the Reference's rule, Items > Generics and Implementations:
/// the container's generic parameters are in scope in its associated items, before the item's
/// own; `Self` is the implementing type in an impl, and the trait's implicit type parameter,
/// index 0, in a trait). A trait's fn's head must be made (a callee the checker picked always
/// has one); an impl's fn's is made here when it is not yet ([`Want::MemberFn`]).
pub fn lower_member<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, key: ItemKey) -> Result<Body<'n>, BodyError> {
    lower_member_into(names, prepared, keys, store, key, None)
}

fn lower_member_into<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, key: ItemKey, into: Option<&mut PortableTys<'static>>) -> Result<Body<'n>, BodyError> {
    // A member's head is made when it is used: a body asked before any check typed a call of
    // it (drop glue, an impl's fn a trait question selected, a trait's provided method) makes
    // it here, by the want that makes it (`member_head_want`), its container's head first.
    if !store.fns.contains_key(&key) && !store.fn_gaps.contains_key(&key) {
        if let Some(want) = super::heads::member_head_want(names, keys, key) {
            store.ensure(names, keys, want).map_err(|error| BodyError::Head { item: want.key(), error })?;
        }
    }
    let head: &FnHead<'n> = match store.fns.get(&key) {
        Some(head) => head,
        // A head refused is its `fn_gaps` row (`Store::member_fn_head` files it and lets the
        // container's head go on): that reason, never "no head" (census r16: 455 fns of tempo
        // were `NoHead` whose head had been refused for a reason of its own).
        None => return Err(refused_head(names, keys, store, key)),
    };
    if !head.has_body {
        return Err(BodyError::NoBody(key));
    }
    let krate = names.crate_of(head.lib).ok_or(BodyError::NoText(head.lib))?;
    // The marks of the member's text, as its head read them (heads/member_fn.rs, heads.rs
    // `trait_members`): the expansion of the call in the container's body that wrote it, or
    // else the container's own (front2 item_site.rs `head_marks`). A derived impl's `default`
    // writes `crate::default::Default::default()`, its `crate` core's only by its mark.
    let marks = match keys.origin(key) {
        // An anonymous constant's text is front2's own rendering `const _: T = EXPR;`
        // (anon.rs `file_asked`), kept in the frame and named under its MODULE, which has no
        // place among the items: no column of marks reaches it, and its words are read at that
        // module, as a rewritten expansion text's are (item_site.rs `marks_of`'s `Ok(None)`).
        // Before, its marks were asked of the module's place and every such constant was
        // refused (`NoPlace`: `size_of::<Self>()` in core's `from_le_bytes`, crypto_bigint's
        // `nlimbs(256)`). OPEN (D122 E5): a def-site word in such an expression keeps no mark.
        Some(Origin::Member { kind: MemberKind::Anon, .. }) => None,
        Some(Origin::Member { kind, name, .. }) => {
            let item_kind = match kind {
                MemberKind::Fn => blaze_surface::ItemKind::Function,
                MemberKind::Const => blaze_surface::ItemKind::Const,
                MemberKind::Type => blaze_surface::ItemKind::TypeAlias,
                // A constructor has no text of its own and no body.
                MemberKind::Ctor | MemberKind::Anon => return Err(BodyError::NoBody(key)),
            };
            super::item_site::head_marks(names, head.lib, name, item_kind, head.text).map_err(|lost| BodyError::Record { item: head.lib, owner: key, why: lost.why() })?
        }
        // A head in `store.fns` under a key front2 never filed as a member: lowered here only
        // by a caller's mistake (a free fn is `lower_fn`'s).
        Some(Origin::Item(_)) | None => {
            let why = Why::Program(format!("{key:?} has a head but is filed as no member of a trait, an impl or a module (a free fn is lowered by `lower_fn`)").into_boxed_str(), Range::new(0, 0));
            return Err(BodyError::Record { item: head.lib, owner: key, why });
        }
    };
    let site = Site {
        item: head.lib,
        owner: key,
        krate,
        module: head.module,
        text: head.text,
        marks,
        params: head.params.clone(),
        consts: head.consts.clone(),
        bounds: head.bounds.clone(),
        self_: head.self_,
        self_assoc: head.self_assoc.clone(),
    };
    let container = head.container;
    let mut body = lower_at(names, prepared, keys, store, site, into)?;
    // The body's own trait is in scope in it, imported or not (own_trait.rs: `impl
    // core::hash::Hash for X { .. self.field.hash(state) .. }`); `traits` stays sorted.
    if let Some(own) = super::own_trait::of_member(store, container).map_err(|e| BodyError::NoHead(e.0))? {
        if let Err(at) = body.traits.binary_search(&own) {
            body.traits.insert(at, own);
            body.blocks.add(own, store.tys.alloc());
        }
    }
    Ok(body)
}

/// Why the member `key` has no head in the store: its head was refused (its `fn_gaps` row,
/// named with its container: [`BodyError::Head`]), else no want made it ([`BodyError::NoHead`]:
/// a key whose container's head was never made, or that is no trait's or impl's member).
fn refused_head<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, key: ItemKey) -> BodyError {
    let Some(why) = store.fn_gaps.get(&key) else { return BodyError::NoHead(key) };
    // A gap is filed only by a container's head being made, so its container has a library
    // key; one that has none is no member front2 filed, and has no head.
    let Some(Origin::Member { container, .. }) = keys.origin(key) else { return BodyError::NoHead(key) };
    let Some(lib) = keys.library_in(names, *container) else { return BodyError::NoHead(key) };
    // `Why` is no `Clone` (it holds a resolver's error): the gap's words, as world_items.rs
    // `fn_row` files a refused head.
    let said = Why::Program(format!("member {key:?}: its head was refused: {why:?}").into_boxed_str(), Range::new(0, 0));
    BodyError::Head { item: lib, error: HeadError::Item(lib, said) }
}

/// Where a body is lowered: its item (a member's is its container), its checker key, text and
/// the names its types are read with.
struct Site<'n> {
    item: LibKey,
    owner: ItemKey,
    krate: usize,
    module: LibKey,
    text: &'n str,
    /// The marks of `text`'s words when a macro wrote it (front2 item_site.rs); `None` for a
    /// text of a file, or one that keeps no origin column (`item_site::marks_of`).
    marks: Option<blaze_scope::crate_roots::TextMarks<'n>>,
    params: Vec<(Name, u32), Global>,
    /// The const generic parameters among `params` ([`Env::consts`]).
    consts: Vec<(u32, PTy), Global>,
    bounds: Vec<ParamBound, Global>,
    self_: SelfTy,
    self_assoc: Vec<(Name, PTy), Global>,
}

/// The gate of a base-less struct expression `S { a: 1, .. }` (RFC 3681,
/// `default_field_values`): on unasked in a blaze edition (the crate's `std` is bstd, D126);
/// in a Rust edition it needs `#![feature(default_field_values)]` at the crate root
/// (`NamesSource::has_feature`, as front2/heads.rs `field_default_allowed`), refused without
/// it, naming the feature, at the expression.
fn defaults_gate<'n, S: NamesSource<'n>>(names: S, krate: usize, typeck: &TypeckBody<'_>) -> Result<(), Why> {
    let Some(i) = typeck.exprs.iter().position(|e| matches!(e, blaze_typeck::Expr::Struct { base: blaze_typeck::StructBase::Defaults, .. })) else {
        return Ok(());
    };
    let range = typeck.expr_ranges.get(i).map_or(Range::new(0, 0), |r| Range::new(r.start, r.end));
    match (names.std_is(krate), names.has_feature(krate, "default_field_values")) {
        (Some(blaze_unit_graph::StdIs::Bstd(_)), _) | (Some(blaze_unit_graph::StdIs::Rust), Some(true)) => Ok(()),
        (Some(blaze_unit_graph::StdIs::Rust), Some(false)) => Err(Why::Program(
            Box::from("a struct expression with `..` and no base needs `#![feature(default_field_values)]` at the crate root in a Rust edition (on unasked in a blaze edition)"),
            range,
        )),
        (Some(blaze_unit_graph::StdIs::Rust), None) => {
            Err(Why::Program(Box::from("a struct expression with `..` and no base: the crate root's features are not answered"), range))
        }
        (None, _) => Err(Why::Program(Box::from("a struct expression with `..` and no base: the crate's edition (which `std` it names) is not answered"), range)),
    }
}

fn lower_at<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, site: Site<'n>, mut into: Option<&mut PortableTys<'static>>) -> Result<Body<'n>, BodyError> {
    let Site { item, owner, krate, module, text, marks, params, consts, bounds, self_, self_assoc } = site;
    // The body's local items keep these marks for their own heads and bodies (locals.rs
    // `LocalHead::marks`).
    let text_marks = marks;
    // Proved once that `text` is a slice of the text its marks are of (rule 74): the lowering
    // takes its text from the proof.
    let marks = match marks {
        Some(m) => Some(blaze_lower::mark_site::ItemMarks::of(text, m).map_err(|miss| BodyError::Record { item, owner, why: Why::Program(format!("{miss}").into_boxed_str(), Range::new(0, 0)) })?),
        None => None,
    };
    // The store's allocator: the body, its types and its block traits are kept for the check.
    let alloc = store.tys.alloc();
    let edition = names.edition_of(krate).ok_or(BodyError::NoText(item))?;
    let atoms = names.cfg_atoms(krate).ok_or(BodyError::NoText(item))?;

    let scoped = ScopedBy { names, module, prepared: Some(prepared) };
    // The crate's host, the one item-level expansion answers from (census 13, brief 13): env,
    // includes, proc macros; `NoHost` refused them all.
    let host = super::body_host::BodyHost::of(names, store.sources, krate, item, module, alloc);
    let body_sources = store.sources;
    let mut expand = ByName::with_names(&scoped, host, alloc);
    let mut values = ModuleValues { names, module, store: &mut *store, keys: &mut *keys, failed: None, krate, item, text, blocks: None };
    let lowered = match marks {
        Some(marks) => blaze_lower::lower_body_expanding_marked(marks, edition, &mut CrateCfg(atoms), &mut values, &mut expand, alloc),
        None => blaze_lower::lower_body_expanding(text, edition, &mut CrateCfg(atoms), &mut values, &mut expand, alloc),
    };
    // D111: a body call's proc-macro read outside the member is a finding at the call, filed
    // through the item host's one filing (body_left.rs), whatever the body's own outcome.
    if let Some(sources) = body_sources {
        let place = super::body_left::body_place(names, krate, item, text.len());
        super::body_left::file_body_left_reads(sources, krate, place, expand.call_reads(), expand.reads(), alloc);
    }
    match values.failed {
        Some(ValuesFail::Resolve(error)) => return Err(BodyError::PatternName { item, error }),
        Some(ValuesFail::Head(error)) => return Err(BodyError::Head { item, error }),
        None => {}
    }
    let lowered = lowered.map_err(|_| BodyError::Cfg(item))?;
    let view = lowered.view();
    let mut findings = Vec::new_in(Global);
    for m in expand.findings() {
        let debug = format!("{:?}", m.why);
        // The chain, then the host's ask when a host refused (`HostError { ask: ProcMacro, .. }`).
        let ask = debug.split_once("ask: ").map(|(_, rest)| crate::class_chain::class_chain(rest, 1));
        let chain = crate::class_chain::class_chain(&debug, 4);
        let cause = match ask {
            Some(ask) => format!("{chain} {ask}").into_boxed_str(),
            None => chain,
        };
        findings.push(BodyFinding::Macro { at: m.at, path: Box::from(m.path.as_str()), why: format!("{}", m.why).into_boxed_str(), cause });
    }
    let record = |why: Why| BodyError::Record { item, owner, why };
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
    // The items declared in the body: kept for the command (their text, keys and blocks).
    let at = super::locals::Enclosing { owner, item, krate, module, text, marks: text_marks };
    store.locals.keep(names, keys, at, &view, alloc).map_err(|error| BodyError::Head { item, error })?;

    // An `async fn`'s coroutine output is its written return type in RETURN position (the
    // Reference, "Async functions": `async fn f() -> T` is `fn f() -> impl Future<Output = T>`;
    // an `impl Trait` in `T` is one of `f`'s opaques): the head's `T`, read from the head
    // ([`super::async_output`]). A free fn's head is made here when it is not yet (a member's
    // was made by `lower_member_into`); lowered again into a finished table (`into`), the head
    // was made by the first lowering, and its absence is refused by the fn's key.
    let async_ret = match super::async_output::written(&view) {
        None => None,
        Some(t) => {
            if super::async_output::of(store, owner).is_none() && into.is_none() {
                store.ensure(names, keys, Want::Fn(item)).map_err(|error| BodyError::Head { item, error })?;
            }
            Some((t, super::async_output::of(store, owner).ok_or(BodyError::NoHead(owner))?))
        }
    };

    // The body's blocks that are modules holding a local type: a written type is read there.
    let blocks: &'n [super::locals::BlockModule] = store.locals.body(owner).map_or(&[], |b| b.blocks);

    let mut last = item;
    let mut typeck = None;
    for _ in 0..MOST_ROUNDS {
        let mut types = BodyTypes {
            // Every word's mark is in the lowered record (`View::mark_of_path`): an expansion's
            // lowered into the body, and the item's own when a macro wrote it
            // (`lower_body_expanding_marked`), so the builder needs no second copy of them.
            builder: Builder { resolver: Paths { names }, heads: &store.maps, marks: None },
            env: Env { module, params: &params, bounds: Bounds::Done(&bounds), self_, self_assoc: &self_assoc, anon: &[], consts: &consts },
            failed: None,
            text,
            misses: Vec::new_in(Global),
            blocks,
            locals: &store.locals,
            owner,
            async_ret,
        };
        // Each round converts into a fresh overlay of the records' table (read in place, never
        // copied): a refused round's types are dropped with it, none reaches `store.tys`, and
        // what the body keeps is only its own types (`OwnTys`), read over `store.tys` again.
        let again_now = into.is_some();
        let (converted, kept) = match into.as_deref_mut() {
            Some(t) => (to_typeck(&view, owner, t, &mut types, alloc), None),
            None => {
                let mut own = PortableTys::over(&store.tys, alloc).map_err(|error| BodyError::Table { item, error })?;
                let converted = to_typeck(&view, owner, &mut own, &mut types, alloc);
                let kept = match converted {
                    Ok(_) => Some(own.keep_in(alloc).map_err(|error| BodyError::Table { item, error })?),
                    Err(_) => None,
                };
                (converted, kept)
            }
        };
        match converted {
            Ok(b) => {
                // Lowered into another table (`lower_again`): the body keeps no types of its own.
                let kept = match kept {
                    Some(k) => k,
                    None => OwnTys::none(alloc),
                };
                typeck = Some((b, kept, core::mem::take(&mut types.misses)));
                break;
            }
            // A fn whose text has no body (an intrinsic's or a foreign fn's declaration, met
            // before its head was made): the table's to say how it is called, never a lowering.
            Err(Refused::NoBody) => return Err(BodyError::NoBody(owner)),
            Err(refused) => match types.failed.take() {
                // Lowered again into a finished table: a head it lacks cannot be added there.
                Some(why @ Why::Need(_)) if again_now => return Err(BodyError::Convert { item, refused, why: Some(why) }),
                Some(Why::Need(k)) => {
                    last = k;
                    // A free fn's head (its const parameters, which type a const argument
                    // written on a path to it: `BodyTypes::arg_param`) is its `Want::Fn`.
                    let need = match (Paths { names }).detail(k) {
                        Some(blaze_surface::ItemDetail::Fn { .. }) => Want::Fn(k),
                        _ => store.need(k),
                    };
                    store.ensure(names, keys, need).map_err(|error| BodyError::Head { item, error })?;
                }
                why => return Err(BodyError::Convert { item, refused, why }),
            },
        }
    }
    let Some((typeck, tys, type_misses)) = typeck else { return Err(BodyError::Rounds { item, last }) };
    defaults_gate(names, krate, &typeck).map_err(|why| BodyError::Record { item, owner, why })?;

    let in_scope = names.traits_in_scope(module, alloc).map_err(|error| BodyError::Traits { item, error })?;
    let mut traits = Vec::with_capacity_in(in_scope.len(), Global);
    traits.extend(in_scope.iter().map(|&k| checker_of(k)));
    traits.sort_unstable();
    traits.dedup();
    // The traits a block's `use` brings into scope there (tracing's `use
    // $crate::__macro_support::Callsite as _;` in the block its macros expand to).
    let blocks = super::block_traits::of(names, &store.locals, owner, module, krate, &traits, alloc);
    drop(view);
    Ok(Body { item, owner, module, lowered, typeck, tys, params, self_, traits, blocks, type_misses })
}
