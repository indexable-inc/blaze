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
use super::heads::{CrateCfg, FnHead, HeadError, Maps, Store, Want};
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

/// A macro call in a body whose expansion is the macro's OWN error (the Reference, "Macros":
/// `compile_error!` "causes compilation to fail with the given error message"; `env!` of an
/// unset variable and `include_str!`/`include_bytes!` of a file that is not there, not UTF-8 or
/// does not lex fail "at compile time" at the call): an error of the program at the call, as
/// rustc reports it there, with the expander's words (blaze-lower by_name.rs
/// `Failure::Diagnostic`'s display, "including `x.json`: no such file").
#[derive(Debug)]
pub struct CallError {
    /// The call's range in the item's text.
    pub at: Range,
    pub path: Box<str, Global>,
    pub why: Box<str, Global>,
}

/// Whether a body call's finding is the macro's own error ([`CallError`]) rather than a call
/// blaze could not take. Only the builtins whose failure is the program's: an include of a
/// file that is missing, not UTF-8 or does not lex (Rust's rules), or whose path leaves what
/// the crate may read (D60 as D111 amends it: "refused by its path and the rule", an error at
/// the call naming the path and D60, blaze-cli include_outside_tests.rs), `compile_error!`, an
/// unset `env!`. Every include failure is named (no `..`), so a new kind is decided. A rule
/// that did not match may be the expander's own gap: it stays a finding of a refused body.
pub fn call_error(why: &blaze_lower::by_name::NotExpanded<'_>) -> bool {
    use blaze_expand::{Diagnostic as D, IncludeFailure as I};
    use blaze_lower::by_name::{Failure, NotExpanded};
    matches!(
        why,
        NotExpanded::Failed(Failure::Diagnostic {
            diagnostic: D::Include { why: I::Missing | I::NotUtf8 | I::DoesNotLex | I::Outside, .. } | D::CompileError { .. } | D::EnvNotDefined { .. },
            ..
        })
    )
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
    /// Every finding of its lowering is a macro call's OWN error ([`call_error`]): the
    /// program's errors at those calls, in the order found, never a refusal ("not checked").
    /// A lowering with any other finding is [`BodyError::Lowered`], these among its findings.
    Calls { item: LibKey, errors: Vec<CallError, Global> },
    /// `to_typeck` refused it; `why` is the type builder's reason when a written type did.
    Convert { item: LibKey, refused: Refused, why: Option<Why>, expanded_type: Option<Result<Box<str>, blaze_lower::TextAt>> },
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

// A pooled body task can return this error through a failed constant evaluation.
const _: () = {
    const fn worker_error<T: Send + 'static>() {}
    worker_error::<BodyError>();
};

impl core::fmt::Display for BodyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BodyError::Const { item, error } => write!(f, "constant {:?} required by this body: {error}", checker_of(*item)),
            BodyError::NoText(k) => write!(f, "fn {k:x}: the names library holds no text for it (a member of a trait or an impl is lowered by `lower_member`)"),
            BodyError::NoHead(k) => write!(f, "member fn {k:?}: its head is not in the store, and no want makes it (it is no fn or constant of a trait or impl whose head front2 made)"),
            BodyError::NoBody(k) => write!(f, "fn {k:?}: its text has no body (a required trait method, an intrinsic or a foreign fn)"),
            BodyError::Cfg(k) => write!(f, "fn {k:x}: its lowering asks for a `cfg` the crate does not answer"),
            BodyError::Generics { item, why } => write!(f, "fn {item:x}: its generic parameters: {why:?}"),
            BodyError::Record { item, owner, why } => write!(f, "body {owner:?} (in the text of {item:x}): its lowered record: {why:?}"),
            BodyError::Lowered { item, findings } => {
                // One line: a refusal carried as text into another body's gap (an auto trait
                // of this fn's future: opaque_auto.rs) or a report's one-line row must still
                // name what was not taken (rule 19); a newline cut the findings off there.
                write!(f, "fn {item:x}: {} things its lowering did not take", findings.len())?;
                for (i, x) in findings.iter().enumerate() {
                    write!(f, "{} {x:?}", if i == 0 { ":" } else { ";" })?;
                }
                Ok(())
            }
            BodyError::Calls { item, errors } => {
                write!(f, "fn {item:x}: {} macro calls failed with their own error", errors.len())?;
                for e in errors {
                    write!(f, "\n  the call of `{}!`: {}", e.path, e.why)?;
                }
                Ok(())
            }
            BodyError::Convert { item, refused, why, expanded_type } => {
                write!(f, "fn {item:x}: to_typeck refused {refused:?}")?;
                if let Some(text) = expanded_type {
                    match text {
                        Ok(text) => write!(f, " in expanded type `{text}`")?,
                        Err(place) => write!(f, " in an expanded type whose source range cannot be read ({place:?})")?,
                    }
                }
                write!(f, " ({why:?})")
            }
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
    /// The body's checker key: a member is found by it (locals.rs `library_item_or_member`).
    owner: ItemKey,
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
        let (names, krate, item, owner, text) = (self.names, self.krate, self.item, self.owner, self.text);
        let planted = self.store.locals.planted();
        let alloc = self.store.tys.alloc();
        let kept = &self.store.locals;
        let keys: &Keys = &*self.keys;
        let blocks = self.blocks.get_or_insert_with(|| super::glob_uses::PatternBlocks::of(names, kept, keys, krate, item, owner, text, alloc));
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
    /// Rewritten item text produced by comptime `inline for` unrolling (`None` for ordinary bodies).
    pub expanded_text: Option<&'n str>,
    /// Original generic owner and concrete ground arguments for a monomorphized `inline for`
    /// specialization (`None` for ordinary bodies).
    pub param_subst: Option<(ItemKey, blaze_own::Vec<PTy, Alloc<'static>>)>,
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
            path_ns: &self.typeck.path_ns,
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
    /// The check's allocator. A constant request survives this lowering round.
    mark_alloc: Alloc<'static>,
    env: Env<'b>,
    failed: Option<Why>,
    /// The expanded spelling for a refusal whose item-relative range points to the call.
    expanded_type: Option<blaze_lower::BodyRange>,
    /// The item's text, which a written type's range is relative to: a [`TypeMiss`] reads its
    /// path there.
    text: &'b str,
    /// Each written type that names nothing (E0412), converted as the error type so the rest of
    /// the body is checked.
    misses: Vec<TypeMiss, Global>,
    /// The items declared in bodies, and whose body this is: a path through a type alias is
    /// expanded only where no item of the body takes its first segment
    /// ([`super::world_paths::AliasPaths`]).
    locals: &'b super::locals::Locals<'b>,
    owner: ItemKey,
    /// An `async fn`'s written return type (its id in the body's view) and the type its head
    /// made of it in return position ([`super::async_output`]): the coroutine's output
    /// `to_typeck` reads, never lowered again in the body's env (which has no return opaques).
    async_ret: Option<(lf::TypeId, PTy)>,
    /// The fn heads made so far (`Store::fns`): a callee's `#[rustc_legacy_const_generics]`
    /// positions ([`Types::legacy_term`]).
    legacy: &'b dyn LegacyPositions,
}

/// A fn's `#[rustc_legacy_const_generics(..)]` positions, by its key: what
/// [`BodyTypes::legacy_term`](Types::legacy_term) reads of the fn heads made so far, as a
/// trait object so the body's types need no lifetime of the heads' texts.
trait LegacyPositions {
    /// The positions as written (heads.rs `FnHead::legacy_const_generics`, empty for a fn
    /// without the attribute); `None` while the fn's head is not made.
    fn legacy_positions(&self, item: ItemKey) -> Option<&[u32]>;
}

impl LegacyPositions for super::rows::Rows<ItemKey, FnHead<'_>, Alloc<'static>> {
    fn legacy_positions(&self, item: ItemKey) -> Option<&[u32]> {
        self.get(&item).map(|head| &head.legacy_const_generics[..])
    }
}

impl<'n, S: NamesSource<'n>> Types for BodyTypes<'_, S> {
    fn ty(&mut self, view: &View<'_>, t: lf::TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        if let Some((at, out)) = self.async_ret {
            if at == t {
                return Ok(out);
            }
        }
        let range = view.type_range(t)?;
        let mut env = self.env;
        // A const expression or type macro that the builder reads as text still needs
        // the block containing that text. Only the item's own bytes index this table.
        // Each written path, including every path inside an expansion, has its own scope.
        if let Some(r) = view.item_relative(range)
            && let Some(body) = self.locals.body(self.owner)
            && let Some(block) = body.blocks.iter().rev().find(|b| b.open <= r.start && r.start < b.close)
        {
            env.module = block.module;
        }
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
                if let Why::Resolve(crate::resolve::ResolveError::Path(e), r) = &why {
                    let names = self.builder.resolver.names;
                    let miss = if range.in_expansion() {
                        // A bare type name has exactly one segment and no children whose
                        // lookup could have failed. Its spelling is in the expansion, while
                        // the diagnostic belongs on the invocation that produced it.
                        if let lf::TypeRef::Path(p) = view.ty(t)?
                            && let [segment] = view.segments(view.path(p)?)?
                            && let lf::Segment { kind: lf::SegKind::Ident, has_args: false, name: Some(n), .. } = lf::Segment::decode(*segment)?
                        {
                            let name = view.name_text(n)?;
                            TypeMiss::of(names, name, *e, Range::new(0, name.len() as u32)).map(|miss| TypeMiss { at: *r, ..miss })
                        } else {
                            None
                        }
                    } else {
                        TypeMiss::of(names, self.text, *e, *r)
                    };
                    if let Some(miss) = miss {
                        self.misses.push(miss);
                        return tys.intern(blaze_typeck::PNode::Error).map_err(Refused::from);
                    }
                }
                if range.in_expansion() && !matches!(why, Why::Need(_) | Why::NeedConst(..)) {
                    self.expanded_type = Some(range);
                }
                self.failed = Some(why);
                Err(Refused::Form { what: "a written type", range: view.site(range)? })
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

    fn method_const_term(&mut self, view: &View<'_>, c: lf::ConstIdx, scope: lf::ScopeId, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let made = self.const_arg_in(view, c, tys, None, Some(scope));
        self.arg_term(made, view, c, None)
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
        self.arg_term(made, view, c, Some(at))
    }

    /// A call argument at a `#[rustc_legacy_const_generics]` position, written as an
    /// expression: rustc (compiler/rustc_ast_lowering/src/expr.rs `lower_legacy_const_generics`)
    /// makes it the fn's next own const generic argument, an anonymous constant, so
    /// `vshrq_n_u32(v, 32 - 12)` is `vshrq_n_u32::<{ 32 - 12 }>(v)` (chacha20 0.10.2's neon
    /// `rotate_left!`, t1545). Valued by the one home for written constants
    /// ([`BodyTypes::const_text_in`]) at the declared type of that parameter: the `j`th
    /// listed position fills own const parameter `j` (the attribute's check admits only const
    /// generics on such a fn: blaze-diagnostics checks/attrs/check.rs). A callee this does not
    /// resolve to a fn with that position listed is `None`, and the checker refuses the
    /// argument by name (blaze-typeck rules/legacy_const.rs).
    fn legacy_term(&mut self, view: &View<'_>, path: blaze_typeck::PathIdx, position: u32, arg: ExprId, tys: &mut PortableTys<'_>) -> Result<Option<ConstTerm>, Refused> {
        let at = view.expr_range(arg)?;
        let range = view.site(at)?;
        let callee = match self.legacy_callee(view, path) {
            Ok(Some(callee)) => callee,
            Ok(None) => return Ok(None),
            Err(why) => {
                self.failed = Some(why);
                return Err(Refused::Form { what: "the callee of a call with a legacy const generic argument", range });
            }
        };
        let Some(positions) = self.legacy.legacy_positions(checker_of(callee)) else {
            // The head is not made. Making it costs this body a whole conversion round (and
            // the seeded path cannot add one), so it is asked for only when the callee's text
            // writes the attribute at all: a fn whose text does not has no listed position,
            // and its argument is an ordinary value. The text only filters; the positions are
            // read from the head once it is made.
            let written = self.builder.resolver.names.text_of(callee).is_some_and(|t| t.text.contains("rustc_legacy_const_generics"));
            if !written {
                return Ok(None);
            }
            self.failed = Some(Why::Need(callee));
            return Err(Refused::Form { what: "the callee of a call with a legacy const generic argument", range });
        };
        let Some(j) = positions.iter().position(|&p| p == position) else { return Ok(None) };
        let want = match super::types::Heads::const_params(self.builder.heads, callee) {
            Some(consts) => consts.iter().find(|(i, _)| *i as usize == j).map(|&(_, t)| t),
            None => {
                self.failed = Some(Why::Need(callee));
                return Err(Refused::Form { what: "the callee of a call with a legacy const generic argument", range });
            }
        };
        let scope = scope_within(view, at)?;
        let made = self.const_text_in(view, at, tys, want, scope, "a legacy const generic argument");
        self.written_term(made, view, at, None).map(Some)
    }

    /// [`Types::const_arg`], with what blaze cannot value yet a GAP ([`const_gap`]): the body
    /// is converted with a hole there and goes on to its check, which reports the gap
    /// (check.rs, `Gap::BodyConst`). Every other refusal stays one (`Why::Need`,
    /// `Why::NeedConst`: the driver makes the record and converts again; a program's error).
    fn const_term(&mut self, view: &View<'_>, c: lf::ConstIdx, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let made = self.const_arg(view, c, tys);
        self.arg_term(made, view, c, None)
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
            Ok(None) => {
                // Replaying a block creates its inner scope again. Its containing
                // scope is the one from which the synthetic member inherits names.
                let scope = match view.expr(len)? {
                    lf::Expr::Block { scope, .. } => view.scope(scope)?.parent,
                    _ => scope_within(view, at)?,
                };
                let local = match scope {
                    Some(scope) => self.body_anon(view.text_at(at)?, view.marks_in(at, self.mark_alloc)?, Some(scope), usize_node, range, tys),
                    None => None,
                };
                match local {
                    Some(made) => made,
                    None => self.builder.const_term(tys, &self.env, text, range, &|tys| super::types::intern(tys, usize_node)),
                }
            }
            Err(why) => Err(why),
        };
        self.term(made, text, "an array repeat length", range)
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
                return self.term(made, name, "a const argument", range);
            }
            Err(why) => return self.term(Err(why), name, "a const argument", range),
            Ok(Some(LocalArg::Type) | None) => {}
        }
        if !is_const && self.builder.resolver.path(self.env.module, false, &[name], Ns::Value).is_err() {
            return Types::ty(self, view, t, tys).map(ConstTerm::Is);
        }
        let made = self.builder.const_term(tys, &self.env, name, range, &|tys| super::types::intern(tys, blaze_typeck::PNode::Infer));
        self.term(made, name, "a const argument", range)
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
/// ([`Why::ConstPath`]). A refusal carrying a cause stays a refusal.
fn const_gap(why: &Why, written: &str) -> Option<ConstGapWhy> {
    match why {
        Why::Unsupported(what, _) if *what == "a constant expression over a generic parameter" => Some(ConstGapWhy::OverGeneric),
        Why::ConstPath(text, _) => blaze_lower::typeck::GapText::quote(written, text).map(ConstGapWhy::AnonInBody),
        _ => None,
    }
}

impl<'n, S: NamesSource<'n>> BodyTypes<'_, S> {
    /// A made const term: the term, its gap ([`const_gap`]), or the refusal `what` at `range`
    /// with the builder's reason kept.
    fn term(&mut self, made: Result<PTy, Why>, written: &str, what: &'static str, range: Range) -> Result<ConstTerm, Refused> {
        match made {
            Ok(t) => Ok(ConstTerm::Is(t)),
            Err(why) => match const_gap(&why, written) {
                Some(gap) => Ok(ConstTerm::Gap(gap)),
                None => {
                    self.failed = Some(why);
                    Err(Refused::Form { what, range })
                }
            },
        }
    }

    /// A made const argument as a term: the term, its gap when the builder's reason is one
    /// ([`const_gap`]), or the refusal. A GAT receiver needs its value for the projection,
    /// so a gap there retains the evaluator's reason and the argument's site.
    fn arg_term(&mut self, made: Result<PTy, Refused>, view: &View<'_>, c: lf::ConstIdx, at: Option<ArgAt>) -> Result<ConstTerm, Refused> {
        let written = view.const_range(c)?;
        self.written_term(made, view, written, at)
    }

    /// [`BodyTypes::arg_term`] of a constant written at `written` of the body (a const
    /// argument's text, or a legacy const generic argument's expression).
    fn written_term(&mut self, made: Result<PTy, Refused>, view: &View<'_>, written: blaze_lower::BodyRange, at: Option<ArgAt>) -> Result<ConstTerm, Refused> {
        let refused = match made {
            Ok(t) => return Ok(ConstTerm::Is(t)),
            Err(refused) => refused,
        };
        let Some(why) = &self.failed else { return Err(refused) };
        let text = match why {
            Why::ConstPath(..) => view.text_at(written)?,
            _ => "",
        };
        let Some(gap) = const_gap(why, text) else { return Err(refused) };
        if let Some(at) = at && gat_segment(view, at)?.is_some() {
            return Err(Refused::Form {
                what: "an unvalued const argument on a generic associated type's segment in a value path (`<T as Tr>::Out::<{ .. }>::x`)",
                range: view.site(written)?,
            });
        }
        self.failed = None;
        self.expanded_type = None;
        Ok(ConstTerm::Gap(gap))
    }

    /// [`Types::const_arg`] of the argument `c`: its parameter's type `want` when known (else
    /// a hole the checker fills), and `scope`, the block its path is written in when known
    /// (an expression naming an item of the body is then a constant of the body,
    /// [`BodyTypes::body_anon`]).
    fn const_arg_in(&mut self, view: &View<'_>, c: lf::ConstIdx, tys: &mut PortableTys<'_>, want: Option<PTy>, scope: Option<lf::ScopeId>) -> Result<PTy, Refused> {
        self.const_text_in(view, view.const_range(c)?, tys, want, scope, "a const argument")
    }

    /// The constant written at `at` of the body (a const argument's text, or a legacy const
    /// generic argument's expression): [`BodyTypes::const_arg_in`]'s reading, refused as
    /// `what`. One home for both, so `f::<{ 32 - 12 }>(a)` and `f(a, 32 - 12)` under
    /// `#[rustc_legacy_const_generics(1)]` are valued alike, as rustc lowers the second to the
    /// first.
    fn const_text_in(&mut self, view: &View<'_>, at: blaze_lower::BodyRange, tys: &mut PortableTys<'_>, want: Option<PTy>, scope: Option<lf::ScopeId>, what: &'static str) -> Result<PTy, Refused> {
        let range = view.site(at)?;
        let source = view.text_at(at)?;
        let marks = view.marks_in(at, self.mark_alloc)?;
        let mut text = source.trim();
        if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            text = inner.trim();
        }
        let want_ty = |tys: &mut PortableTys<'_>| match want {
            Some(t) => Ok(t),
            None => super::types::intern(tys, blaze_typeck::PNode::Infer),
        };
        let made = match self.const_variant(source, text, &marks, scope, range) {
            Err(why) => Err(why),
            Ok(Some((adt, index))) => return tys.variant_value(checker_of(adt), index).map_err(Refused::from),
            Ok(None) => {
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
                let local = self.body_anon(source, marks, scope, node.unwrap_or(blaze_typeck::PNode::Infer), range, tys);
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
                Err(Refused::Form { what, range })
            }
        }
    }

    /// A variant path has its own const representation. Its first token uses the
    /// same hygiene as the scalar expression we would otherwise copy.
    fn const_variant(
        &self,
        source: &str,
        text: &str,
        marks: &[(u32, blaze_lower::mark_site::TextMark)],
        scope: Option<lf::ScopeId>,
        range: Range,
    ) -> Result<Option<(LibKey, u32)>, Why> {
        if !text.contains("::") {
            return Ok(None);
        }
        let abs = text.starts_with("::");
        let rest = text.strip_prefix("::").unwrap_or(text);
        let mut segments = blaze_own::AVec::new_in(self.mark_alloc);
        for word in rest.split("::").map(str::trim) {
            let ident = word.strip_prefix("r#").unwrap_or(word);
            if word != "$crate" && (ident.is_empty() || !ident.chars().all(|c| c == '_' || c.is_alphanumeric())) {
                return Ok(None);
            }
            segments.push(word);
        }
        let first = segments[0];
        let offset = blaze_scope::expansion_origins::offset_in(source, first)
            .ok_or(Why::Unsupported("the first segment of an enum constant is outside the argument text", range))?;
        let mark = marks.binary_search_by_key(&offset, |m| m.0).ok().map(|i| marks[i].1);
        let site = super::item_site::marked_site(self.builder.resolver.names, mark, first, abs, self.env.module, range)?;
        let mut module = site.module;
        let mut dollar = site.dollar_crate;
        let mut absolute = abs;
        if !abs {
            let (row, hit) = super::scope_table::value_prefix(
                &self.builder.resolver, self.locals, self.owner, self.env.module, scope.map(|s| s.0), first, false, mark, self.mark_alloc,
            ).map_err(|gap| Why::Unsupported(gap.form, range))?;
            if let Some(hit) = hit {
                let mut through = hit.path;
                through.extend_from_slice(&segments[1..]);
                segments = through;
                module = hit.at;
                absolute = hit.abs;
                dollar = hit.dollar;
            } else if let Some(row) = row {
                let Some(at) = row.module else { return Ok(None) };
                module = at;
            }
        }
        let found = self.builder.resolver.path_with(module, dollar, absolute, &segments, crate::resolve::Ns::Value);
        Ok(match found {
            Ok(crate::resolve::Target { consumed, kind: crate::resolve::Kind::Variant { adt, index }, .. }) if consumed as usize == segments.len() => Some((adt, index)),
            _ => None,
        })
    }

    /// Whether the constant expression `text`, written in block `scope` of this body, names an
    /// item declared in the body ([`super::locals::Locals::find`] in either namespace, from
    /// that block outwards). Each identifier is asked (a number's digits and suffix are one
    /// token, never an identifier); one that only a block's glob `use` gives is not seen.
    fn names_body_item(&self, text: &str, scope: u32) -> bool {
        use super::locals::LocalNs;
        identifiers(text).any(|ident| self.locals.find(self.owner, Some(scope), ident, LocalNs::Value).is_some() || self.locals.find(self.owner, Some(scope), ident, LocalNs::Type).is_some())
    }

    /// File a constant with its enclosing scope when it refers to a block item or carries
    /// expansion marks. The marks start relative to `source`, including its whitespace.
    fn body_anon(&self, source: &str, mut marks: blaze_own::AVec<'static, (u32, blaze_lower::mark_site::TextMark)>, scope: Option<lf::ScopeId>, ty: blaze_typeck::PNode, range: Range, tys: &mut PortableTys<'_>) -> Option<Result<PTy, Why>> {
        let text = source.trim();
        let inner = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')).map_or(text, str::trim);
        let local = scope.is_some_and(|scope| self.names_body_item(inner, scope.0));
        if !local && marks.is_empty() {
            return None;
        }
        if !local {
            // A literal, inference hole or standalone const parameter already has a
            // contextual reading. None of these has an item path to resolve.
            let digits = inner.strip_prefix('-').map_or(inner, str::trim_start);
            if matches!(inner, "_" | "true" | "false")
                || self.env.params.iter().any(|(n, _)| *n == Name::of(inner))
                || parser::LexedStr::single_token(parser::Edition::CURRENT, digits).is_some_and(|(kind, _)| kind.is_literal())
            {
                return None;
            }
            // An expression that names nothing (`12 - 4`, chacha20's `32 - $r` from a
            // `literal` fragment): its marks place no name, so it is read where an unmarked
            // one is, with no block. Hygiene (the Reference, "Macros by example", "Hygiene")
            // is about where names resolve. Before (tempo t1935d, 7 lines; ladder
            // legacy_const_anon_arg_run), such an argument had no path to take a block from
            // (`scope_within`) and was refused "whose enclosing block is missing".
            if identifiers(inner).next().is_none() {
                return None;
            }
        }
        let Some(scope) = scope else {
            return Some(Err(Why::Unsupported("an anonymous constant with marked tokens whose enclosing block is missing", range)));
        };
        // Keep braces belonging to the expression: its block may declare its own items.
        let leading = (source.len() - source.trim_start().len()) as u32;
        marks.retain_mut(|(start, _)| {
            if *start < leading || *start - leading >= text.len() as u32 {
                return false;
            }
            *start -= leading;
            true
        });
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
        let bits = match super::anon::body_value(self.builder.heads, ask.carry_marks(marks), range) {
            Ok(bits) => bits,
            Err(why) => return Some(Err(why)),
        };
        Some(super::types::intern(tys, ty).and_then(|t| super::types::intern(tys, blaze_typeck::PNode::ConstValue { ty: t, bits })))
    }

    /// The free fn a call's plain-path callee `path` names, resolved as [`BodyTypes::arg_param`]
    /// resolves a path (the module's names at the path's own spelling and site): `None` for
    /// any other item, a path a generic parameter or a block of the body may name, or a path
    /// the module does not resolve whole. Those are the checker's to read (blaze-typeck
    /// rules/legacy_const.rs reads its own callee).
    fn legacy_callee(&self, view: &View<'_>, path: blaze_typeck::PathIdx) -> Result<Option<LibKey>, Why> {
        use crate::resolve::{Kind, Ns};
        let Ok((abs, texts, segs, range)) = super::types::path_texts_of(view, path, false) else { return Ok(None) };
        if texts.is_empty() || segs.iter().any(|s| !matches!(s.kind, lf::SegKind::Ident | lf::SegKind::Crate | lf::SegKind::DollarCrate | lf::SegKind::SelfMod | lf::SegKind::Super)) {
            return Ok(None);
        }
        if self.env.params.iter().any(|(n, _)| *n == Name::of(texts[0])) {
            return Ok(None);
        }
        if self.locals.head(self.owner).is_some() {
            return Ok(None);
        }
        if let Some(b) = self.locals.body(self.owner) {
            if b.rows.iter().any(|r| r.name == texts[0] || r.name == super::glob_uses::GLOB) {
                return Ok(None);
            }
        }
        let site = super::item_site::written_site(&self.builder.resolver, self.builder.marks, self.env.module, view, path, abs, texts[0], range)?;
        let Ok(found) = self.builder.resolver.path_with(site.module, site.dollar_crate, abs, &texts[..], Ns::Value) else { return Ok(None) };
        if found.consumed as usize != texts.len() || !matches!(found.kind, Kind::Fn) {
            return Ok(None);
        }
        Ok(Some(found.key))
    }

    /// The declared type of the const parameter that the argument at `at` fills, when the
    /// path's segment names an item this body's module resolves (the Reference, "Generic
    /// parameters", "Const generics": an argument's type is its parameter's): a free fn's, an
    /// ADT's, a trait's (its arguments follow `Self`) or a type alias's const parameters, as
    /// its head filed them ([`super::types::Heads::const_params`]). A head not made yet is
    /// [`Why::Need`] (the driver makes it and converts again, `lower_at`). `None`: the type is
    /// the checker's to find: a value member after an anchor or a generic parameter, an associated
    /// item (`Ty::f::<{..}>`), or a path whose first segment a block of the body may name (a
    /// local item, a named or glob import), which the module's names do not see.
    fn arg_param(&self, view: &View<'_>, at: ArgAt) -> Result<Option<PTy>, Why> {
        use crate::resolve::{Kind, Ns};
        use super::types::{Bounded, Heads};
        // Resolve the anchor's trait at its own spelling, including a macro's `$crate`.
        // Both ArgAt and the GAT head number own parameters from zero and erase lifetimes.
        if let Some((path, name)) = gat_segment(view, at)? {
            let (abs, names, _, r) = super::types::path_texts(view, path)?;
            let Bounded::Trait(owner) = self.builder.bound_target(&self.env, view, path, abs, &names, r)? else { return Ok(None) };
            let heads = self.builder.heads;
            if !heads.trait_has_assoc(owner, name).ok_or(Why::Need(owner))? {
                return Ok(None);
            }
            let Some(params) = heads.assoc_const_params(owner, name) else {
                return if heads.trait_supers(owner).is_none() {
                    Err(Why::Need(owner))
                } else {
                    Err(Why::Unsupported("a generic associated type whose declared const parameters are missing", r))
                };
            };
            return Ok(params.iter().find(|(i, _)| *i == at.index).map(|&(_, ty)| ty));
        }
        let Ok((abs, texts, segs, range)) = super::types::path_texts_of(view, at.path, false) else { return Ok(None) };
        let seg = at.segment as usize;
        if seg >= texts.len() || segs.iter().take(seg + 1).any(|s| !matches!(s.kind, lf::SegKind::Ident | lf::SegKind::Crate | lf::SegKind::DollarCrate | lf::SegKind::SelfMod | lf::SegKind::Super)) {
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
        let site = super::item_site::written_site(&self.builder.resolver, self.builder.marks, self.env.module, view, at.path, abs, texts[0], range)?;
        let Ok(found) = self.builder.resolver.path_with(site.module, site.dollar_crate, abs, upto, ns) else { return Ok(None) };
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

/// The trait path and associated type of an argument on `Out` in `<T as Tr>::Out::<A>::x`.
fn gat_segment(view: &View<'_>, at: ArgAt) -> Result<Option<(blaze_typeck::PathIdx, Name)>, lf::BadRow> {
    if at.segment == 1
        && let [first, assoc, _] = view.segments(view.path(at.path)?)?
        && let lf::Segment { kind: lf::SegKind::Anchor, trait_: Some(tr), .. } = lf::Segment::decode(*first)?
        && let Some(name) = lf::Segment::decode(*assoc)?.name
        && let lf::TypeRef::Path(path) = view.ty(tr)?
    {
        return Ok(Some((path, Name(view.name(name)?))));
    }
    Ok(None)
}

/// The block of the first path written inside `at` (an expression's range), which is the
/// block the expression's names are read in; `None` when no path is written there (the
/// expression names nothing).
/// The words of the constant expression `text` that may be names: each run of identifier
/// bytes not starting with a digit (a number's digits and suffix are one token, never an
/// identifier). A word inside a string literal is one too: a superset, never short of a name.
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    let mut i = 0;
    core::iter::from_fn(move || {
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
            if let Some(ident) = text.get(start..i) {
                return Some(ident);
            }
        }
        None
    })
}

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

/// Where a lowering's types go: THE ONE SEAM between the paths (rule 78), each tested equal
/// by the cost test (crates/blaze-ladder/tests/front2_tables_cost.rs: equal answers).
enum TypesInto<'t> {
    /// A fresh overlay of the store's table each round; the body keeps its own part.
    Overlay,
    /// The store's own table (the seeded path: a body's private store over stable columns).
    Store,
    /// Another, finished table (`lower_again`).
    Again(&'t mut PortableTys<'static>),
}

/// Lowers `body`'s fn again, its types into `into` (a copy of `store.tys` made after every
/// head it needs: a head it would still need is a refusal), for a driver that reads every
/// body and record from one table (the run path's seam). The returned body's own `tys` is
/// empty: its types are `into`'s.
pub fn lower_again<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, body: &Body<'n>, into: &mut PortableTys<'static>) -> Result<Body<'n>, BodyError> {
    if let Some(text) = body.expanded_text {
        let subst = body.param_subst.as_ref().map(|(orig, args)| (*orig, args.as_slice()));
        return lower_body_with_text(names, prepared, keys, store, body.item, body.owner, text, subst, Some(into));
    }
    match keys.library_in(names, body.owner) {
        Some(lib) => lower_fn_into(names, prepared, keys, store, lib, Some(into)),
        None => lower_member_into(names, prepared, keys, store, body.owner, Some(into)),
    }
}

/// Lowers `owner`'s body from rewritten item text `text` (produced by `inline for` unrolling),
/// optionally substituting generic parameter indices with concrete `subst` arguments from `orig`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_body_with_text<'n, S: NamesSource<'n>>(
    names: S,
    prepared: &Prepares<'n>,
    keys: &mut Keys,
    store: &mut Store<'n>,
    item: LibKey,
    owner: ItemKey,
    text: &'n str,
    param_subst: Option<(ItemKey, &[PTy])>,
    into: Option<&mut PortableTys<'static>>,
) -> Result<Body<'n>, BodyError> {
    let head_key = param_subst.map_or(owner, |(orig, _)| orig);
    let head = store.fns.get(&head_key).ok_or(BodyError::NoHead(head_key))?;
    let krate = names.crate_of(head.lib).ok_or(BodyError::NoText(head.lib))?;
    let alloc = *head.params.allocator();
    let container = head.container;
    let site = Site {
        item,
        owner,
        krate,
        module: head.module,
        text,
        marks: None,
        params: head.params.to_vec_in(alloc),
        consts: head.consts.to_vec_in(alloc),
        bounds: head.bounds.to_vec_in(alloc),
        self_: head.self_,
        self_assoc: head.self_assoc.to_vec_in(alloc),
    };
    let subst_slice = param_subst.map(|(_, args)| args);
    let mut body = lower_at(names, prepared, keys, store, site, into, subst_slice)?;
    body.expanded_text = Some(text);
    if let Some((orig, args)) = param_subst {
        let store_alloc = store.tys.alloc();
        let mut kept_args = blaze_own::Vec::with_capacity_in(args.len(), store_alloc);
        kept_args.extend_from_slice(args);
        body.param_subst = Some((orig, kept_args));
        body.params.clear();
        if let Some(spec_head) = store.fns.get(&owner) {
            body.self_ = spec_head.self_;
        }
    }
    if let Some(own) = super::own_trait::of_member(store, container).map_err(|e| BodyError::NoHead(e.0))? {
        if let Err(at) = body.traits.binary_search(&own) {
            body.traits.insert(at, own);
            body.blocks.add(own, store.tys.alloc());
        }
    }
    Ok(body)
}

fn lower_fn_into<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, item: LibKey, into: Option<&mut PortableTys<'static>>) -> Result<Body<'n>, BodyError> {
    let krate = names.crate_of(item).ok_or(BodyError::NoText(item))?;
    let owner = checker_of(item);
    // The body's tokens keep their expansion marks. Its generic names, types and bounds
    // have already been resolved at their declaration in the function head.
    let (module, text, marks) = match store.locals.head(owner) {
        Some(h) => (h.module, h.text, h.marks),
        None => {
            let it = names.text_of(item).ok_or(BodyError::NoText(item))?;
            let marks = super::item_site::item_marks(names, item, it.text).map_err(|e| BodyError::Generics { item, why: e.why() })?;
            (it.module, it.text, marks)
        }
    };
    if !store.fns.contains_key(&owner) && into.is_none() {
        store.ensure(names, keys, Want::Fn(item)).map_err(|error| BodyError::Head { item, error })?;
    }
    let head = store.fns.get(&owner).ok_or(BodyError::NoHead(owner))?;
    let alloc = *head.params.allocator();
    let site = Site {
        item, owner, krate, module, text, marks,
        params: head.params.to_vec_in(alloc),
        consts: head.consts.to_vec_in(alloc),
        bounds: head.bounds.to_vec_in(alloc),
        self_: head.self_,
        self_assoc: head.self_assoc.to_vec_in(alloc),
    };
    lower_at(names, prepared, keys, store, site, into, None)
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
        Some(Origin::Member { kind: MemberKind::Anon, .. }) => store.anon_marks.get(&key).copied(),
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
    let alloc = *head.params.allocator();
    let site = Site {
        item: head.lib,
        owner: key,
        krate,
        module: head.module,
        text: head.text,
        marks,
        params: head.params.to_vec_in(alloc),
        consts: head.consts.to_vec_in(alloc),
        bounds: head.bounds.to_vec_in(alloc),
        self_: head.self_,
        self_assoc: head.self_assoc.to_vec_in(alloc),
    };
    let container = head.container;
    let mut body = lower_at(names, prepared, keys, store, site, into, None)?;
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

/// How many bodies a local item can be declared inside, one in another, for [`body_at`]: a
/// chain longer than this is no program's (each level is a body's block in its enclosing
/// body's text); one longer is not placed (`line!()` refused by name), never cut short.
const MOST_LOCAL_LEVELS: usize = 64;

/// Where the body lowered from `text` under `item` (checker key `owner`) stands in its file
/// (body_host.rs [`super::body_host::BodyAt`]): the library body the names place, the free fn
/// or member whose body `text` is ([`super::locals::library_item_or_member`]: a member by its
/// container, kind and name), at 0; a local item's, its outermost
/// enclosing library body's, at the local's place in it, walked out through each enclosing
/// body's [`super::locals::LocalHead::range`] (rule 71: item-relative to the end). A local a
/// macro call wrote ([`super::locals::LocalHead::copied`]) is placed at that call: every call
/// in its copy is (core `line!`: "the first macro invocation leading up to the expansion"), so
/// the offsets inside it are dropped. `None`: no library body (an anonymous constant's
/// rendering, a chain past [`MOST_LOCAL_LEVELS`]).
fn body_at<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, item: LibKey, owner: ItemKey, text: &str) -> Option<super::body_host::BodyAt> {
    let mut key = owner;
    let mut off = 0u32;
    let mut fixed = false;
    for _ in 0..MOST_LOCAL_LEVELS {
        let Some(h) = store.locals.head(key) else {
            // A library body: the lowered body itself (a free fn: its item; a member: the member
            // whose text it is, never its container, whose first byte is not the member's), or
            // the enclosing body a local was declared in, lowered from its own item's or
            // member's text.
            // A MEMBER by its container, kind and name (locals.rs `library_item_or_member`), never
            // by its text's slice: tempo's check refused `line!()` in 167 bodies, every one a
            // method, none of its free fns (tracing's callsite statics in `handle_tip`, `run`,
            // ..: rt-loop a2210; blaze-cli engine_check_tests.rs
            // `engine_check_checks_a_tracing_event_in_a_method`).
            let lib = if key == owner {
                match keys.library_in(names, owner) {
                    Some(lib) if lib == item => item,
                    _ => super::locals::library_item_or_member(names, keys, item, owner, text)?,
                }
            } else {
                match keys.library_in(names, key) {
                    Some(lib) => lib,
                    None => {
                        let head = store.fns.get(&key)?;
                        super::locals::library_item_or_member(names, keys, head.lib, key, head.text)?
                    }
                }
            };
            return Some(super::body_host::BodyAt { lib, off, fixed });
        };
        if h.copied {
            off = h.range.start;
            fixed = true;
        } else {
            off = off.checked_add(h.range.start)?;
        }
        key = h.enclosing;
    }
    None
}

fn lower_at<'n, S: NamesSource<'n>>(names: S, prepared: &Prepares<'n>, keys: &mut Keys, store: &mut Store<'n>, site: Site<'n>, mut into: Option<&mut PortableTys<'static>>, param_subst: Option<&[PTy]>) -> Result<Body<'n>, BodyError> {
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

    // The body's blocks the names library made modules: a call in one, written by hand or by
    // an expansion, reads its path there (scoped.rs `body_blocks`). Only for a text that is the
    // library's item's or member's own (a local item's body, an anonymous constant's
    // rendering: none).
    // A member by its key, never by its text's slice (locals.rs `library_item_or_member`: a
    // method's blocks had no modules before).
    let blocks = match (super::locals::library_item_or_member(names, keys, item, owner, text), names.crate_key(krate)) {
        (Some(body), Some(crate_key)) => super::scoped::body_blocks(crate_key, body, text, alloc),
        _ => None,
    };
    let scoped = ScopedBy { names, module, prepared: Some(prepared), blocks: blocks.as_ref().map(super::scoped::BodyBlocks::modules) };
    // The crate's host, the one item-level expansion answers from (census 13, brief 13): env,
    // includes, proc macros; `NoHost` refused them all.
    // Placed by the body it is (a local item's: its enclosing item's), so `line!()` in a local
    // static's initializer counts where the static is (tempo rt-loop a2000: tracing's callsite
    // statics, checked as bodies of their own, were placed at their local key: refused).
    let at = body_at(names, keys, store, item, owner, text);
    let host = super::body_host::BodyHost::of_body(names, store.sources, krate, item, at, module, alloc);
    let body_sources = store.sources;
    let mut expand = ByName::with_names(&scoped, host, alloc);
    let mut values = ModuleValues { names, module, store: &mut *store, keys: &mut *keys, failed: None, krate, item, owner, text, blocks: None };
    let extensions = names.blaze_extensions(krate);
    let lowered = match marks {
        Some(marks) => blaze_lower::lower_body_expanding_marked_with_extensions(marks, edition, extensions, &mut CrateCfg(atoms), &mut values, &mut expand, alloc),
        None => blaze_lower::lower_body_expanding_with_extensions(text, edition, extensions, &mut CrateCfg(atoms), &mut values, &mut expand, alloc),
    };
    // D111: a body call's proc-macro read outside the member is a finding at the call, filed
    // through the item host's one filing (body_left.rs), whatever the body's own outcome.
    if let Some(sources) = body_sources {
        let place = super::body_left::body_place(names, krate, item, text.len());
        super::body_left::file_body_left_reads(sources, krate, place, expand.call_reads(), expand.reads(), alloc);
        // Its includes too, placed from the body's file as the body host placed them, the file
        // asked only at the first include read (body_left.rs `file_body_left_includes`: over
        // records it reads the crate's items section whole). A body with no file had every
        // include refused by its host (`HostAsk::Include`): no read.
        let calling = || sources.folder(krate).and_then(|package| super::body_host::body_file(names, krate, item)?.package.in_package(package, alloc));
        super::body_left::file_body_left_includes(sources, krate, place, calling, expand.call_reads(), expand.reads(), alloc);
    }
    match values.failed {
        Some(ValuesFail::Resolve(error)) => return Err(BodyError::PatternName { item, error }),
        Some(ValuesFail::Head(error)) => return Err(BodyError::Head { item, error }),
        None => {}
    }
    let lowered = lowered.map_err(|_| BodyError::Cfg(item))?;
    let view = lowered.view();
    let mut findings = Vec::new_in(Global);
    // The calls whose finding is the macro's own error ([`call_error`]): each also stays a
    // finding below, so a lowering with any other finding is refused exactly as before.
    let mut calls: Vec<CallError, Global> = Vec::new_in(Global);
    for m in expand.findings() {
        if call_error(&m.why) {
            // At its site (rule 71): the call itself, or the outermost call in the item's text
            // whose expansion wrote it (`dep::missing_of!()` writing `env!("UNSET")`), the range
            // the lowering's own note of it is mapped to below. Before, the raw range (past the
            // item's text) never equalled that site, so the note stayed a finding of its own and
            // an unset `env!` a macro wrote printed "not checked" (check_body_env.rs, t82).
            let at = view.call_site(m.at).map_err(|t| BodyError::Record { item, owner, why: Why::from(t) })?;
            calls.push(CallError { at, path: Box::from(m.path.as_str()), why: format!("{}", m.why).into_boxed_str() });
        }
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
        // The lowering's own note of a call whose expansion failed, at the very range of a
        // call whose failure is the macro's own error (above): the same error said twice, so
        // it is that call's, not a finding of its own (D189: an `include_str!` of a missing
        // file is the program's error at the call; the twin, any other unexpanded call, has
        // no call error at its range and stays a finding).
        if u.why == lf::Why::ExpansionFailed && calls.iter().any(|c| c.at == range) {
            continue;
        }
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
        // Every finding a call's own error: the program's errors at its calls.
        if calls.len() == findings.len() {
            return Err(BodyError::Calls { item, errors: calls });
        }
        return Err(BodyError::Lowered { item, findings });
    }
    // The items declared in the body: kept for the command (their text, keys and blocks).
    let at = super::locals::Enclosing { owner, item, krate, module, text, marks: text_marks };
    store.locals.keep(names, keys, at, &view, alloc).map_err(|error| BodyError::Head { item, error })?;

    // An `async fn`'s coroutine output is its written return type in RETURN position (the
    // Reference, "Async functions": `async fn f() -> T` is `fn f() -> impl Future<Output = T>`;
    // an `impl Trait` in `T` is one of `f`'s opaques): the head's `T`, read from the head
    // ([`super::async_output`]). Both free and member bodies enter with their head made.
    let async_ret = match super::async_output::written(&view) {
        None => None,
        Some(t) => Some((t, super::async_output::of(store, owner).ok_or(BodyError::NoHead(owner))?)),
    };

    // Where the body's written types are read when no block of its own is a module: a local
    // item's at the module its HEAD read them at (locals.rs `LocalHead::names_at`: its block's
    // module, whose chain reaches the blocks around it and then the body's module), not its
    // enclosing body's module. Its signature is lowered again with its body, and a signature's
    // path has no block scope for the blocks' imports to answer (block_type_uses.rs
    // `through_use`): alloc's `custom_owner_from_box` writes `use core::ptr::NonNull;` and then
    // `unsafe fn drop_box_raw<T: ?Sized>(ptr: NonNull<T>)` in its block, whose head resolved
    // `NonNull` there while its body read it at `alloc::io::error`: E0412 (ladder rungs f3-dyn,
    // x0-ref-11, x0-ref-12). Any other body: its own module.
    let types_at = store.locals.head(owner).map_or(module, |h| h.names_at);
    note_body_record_field_names(&view, &store.maps);
    let mut last = item;
    let mut typeck = None;
    for _ in 0..MOST_ROUNDS {
        let mut types = BodyTypes {
            mark_alloc: alloc,
            // Every word's mark is in the lowered record (`View::mark_of_path`): an expansion's
            // lowered into the body, and the item's own when a macro wrote it
            // (`lower_body_expanding_marked`), so the builder needs no second copy of them.
            builder: Builder { resolver: Paths { names }, heads: &store.maps, marks: None },
            // A written type's first segment is asked of the blocks' named imports first
            // (super::block_type_uses): a block holding only `use`s is no module to read at.
            env: Env {
                module: types_at,
                params: &params,
                bounds: Bounds::Done(&bounds),
                self_,
                self_assoc: &self_assoc,
                anon: &[],
                consts: &consts,
                body: Some(super::block_type_uses::BodyScope { locals: &store.locals, owner, module, alloc }),
            },
            failed: None,
            expanded_type: None,
            text,
            misses: Vec::new_in(Global),
            locals: &store.locals,
            owner,
            async_ret,
            legacy: &store.fns,
        };
        // Each round converts into a fresh overlay of the records' table (read in place, never
        // copied): a refused round's types are dropped with it, none reaches `store.tys`, and
        // what the body keeps is only its own types (`OwnTys`), read over `store.tys` again.
        // THE SEEDED PATH (a body's private store, seeded from frozen head records: tables
        // task.rs `check_body`) has NO OVERLAY: the store's table is over stable columns and
        // only this body's check reads it, so the body is lowered straight into it, as
        // `lower_again` lowers into its table, and one table numbers every type once: a head
        // imported into it during the check gets an id no body type has, and the check's view
        // (`OwnTys::in_base`, live) reads it. A refused round's types stay in that private
        // table, unread. The lowering path keeps its overlay (`TypesInto`, the one seam).
        let again_now = into.is_some();
        let target = match (into.as_deref_mut(), store.seeds.is_some()) {
            (Some(t), _) => TypesInto::Again(t),
            (None, true) => TypesInto::Store,
            (None, false) => TypesInto::Overlay,
        };
        let (converted, kept) = match target {
            TypesInto::Again(t) => {
                let mut converted = to_typeck(&view, owner, t, &mut types, alloc);
                if let (Ok(b), Some(subst_args)) = (&mut converted, param_subst) {
                    if let Err(err) = super::inline_for::subst_typeck_body(b, t, subst_args) {
                        converted = Err(Refused::Check(err));
                    }
                }
                (converted, None)
            }
            TypesInto::Store => {
                let mut converted = to_typeck(&view, owner, &mut store.tys, &mut types, alloc);
                if let (Ok(b), Some(subst_args)) = (&mut converted, param_subst) {
                    if let Err(err) = super::inline_for::subst_typeck_body(b, &mut store.tys, subst_args) {
                        converted = Err(Refused::Check(err));
                    }
                }
                let kept = converted.is_ok().then(|| OwnTys::in_base(alloc));
                (converted, kept)
            }
            TypesInto::Overlay => {
                let mut own = PortableTys::over(&store.tys, alloc).map_err(|error| BodyError::Table { item, error })?;
                let mut converted = to_typeck(&view, owner, &mut own, &mut types, alloc);
                if let (Ok(b), Some(subst_args)) = (&mut converted, param_subst) {
                    if let Err(err) = super::inline_for::subst_typeck_body(b, &mut own, subst_args) {
                        converted = Err(Refused::Check(err));
                    }
                }
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
                Some(why @ Why::Need(_)) if again_now => return Err(BodyError::Convert { item, refused, why: Some(why), expanded_type: None }),
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
                why => {
                    // The caller's site owns its parameters on the heap. Its allocator can
                    // also own the spelling after the worker drops the lowering and store.
                    let expanded_type = types.expanded_type.map(|at| view.text_at(at).map(|text| {
                        let bytes = text.as_bytes().to_vec_in(*params.allocator());
                        std::string::String::from_utf8(bytes).expect("copied from a str").into_boxed_str()
                    }));
                    return Err(BodyError::Convert { item, refused, why, expanded_type });
                }
            },
        }
    }
    let Some((typeck, tys, type_misses)) = typeck else { return Err(BodyError::Rounds { item, last }) };
    defaults_gate(names, krate, &typeck).map_err(|why| BodyError::Record { item, owner, why })?;
    if Store::view_has_record_type(&view) {
        let body_env = Env { module, params: &params, bounds: Bounds::Done(&bounds), self_, self_assoc: &self_assoc, anon: &[], consts: &consts, body: None };
        store.register_record_defaults(names, keys, item, &body_env, None, &view).map_err(|why| BodyError::Record { item, owner, why })?;
    }

    let in_scope = names.traits_in_scope(module, alloc).map_err(|error| BodyError::Traits { item, error })?;
    let mut traits = Vec::with_capacity_in(in_scope.len(), Global);
    traits.extend(in_scope.iter().map(|&k| checker_of(k)));
    traits.sort_unstable();
    traits.dedup();
    // The traits a block's `use` brings into scope there (tracing's `use
    // $crate::__macro_support::Callsite as _;` in the block its macros expand to).
    let blocks = super::block_traits::of(names, &store.locals, owner, module, krate, &traits, alloc);
    drop(view);
    Ok(Body { item, owner, module, lowered, typeck, tys, params, self_, traits, blocks, type_misses, expanded_text: None, param_subst: None })
}

/// Records the field name spellings of every structural record literal and record pattern in
/// `view` into `maps.record_field_names` so `inline for` unrolling can spell anonymous record
/// fields even across function boundaries.
fn note_body_record_field_names(view: &View<'_>, maps: &super::heads::Maps) {
    use super::types::Heads as _;
    for i in 0..view.exprs.len() {
        let Ok(idx) = u32::try_from(i) else { break };
        let Ok(blaze_lower::Expr::Record { fields, .. }) = view.expr(ExprId(idx)) else { continue };
        let Ok(rows) = view.rows(fields) else { continue };
        for &row in rows {
            let Ok(fi) = blaze_lower::form::FieldInit::decode(row) else { continue };
            let Ok(hash) = view.name(fi.name) else { continue };
            let Ok(r) = view.name_range(fi.name) else { continue };
            let Ok(raw) = view.text_at(r) else { continue };
            let text = raw.strip_prefix("r#").unwrap_or(raw);
            if !text.is_empty() {
                maps.note_record_field_name(Name(hash), text);
            }
        }
    }
    for i in 0..view.pats.len() {
        let Ok(idx) = u32::try_from(i) else { break };
        let Ok(blaze_lower::Pat::Record { fields, .. }) = view.pat(blaze_lower::PatId(idx)) else { continue };
        let Ok(rows) = view.rows(fields) else { continue };
        for &row in rows {
            let Ok(fp) = blaze_lower::form::FieldPat::decode(row) else { continue };
            let Ok(hash) = view.name(fp.name) else { continue };
            let Ok(r) = view.name_range(fp.name) else { continue };
            let Ok(raw) = view.text_at(r) else { continue };
            let text = raw.strip_prefix("r#").unwrap_or(raw);
            if !text.is_empty() {
                maps.note_record_field_name(Name(hash), text);
            }
        }
    }
}
