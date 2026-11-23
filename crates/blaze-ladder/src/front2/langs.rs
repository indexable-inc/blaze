//! The checker's lang items by the names `#[lang = ".."]` gives them in the library's
//! `tables.langs` ([`super::index::Index::lang`]): the one map from the checker's
//! [`LangItem`] to a written lang name (front.rs's `trait_lang` and its `lang_paths` arms go
//! at the switch). A checker lang item no `#[lang]` names (the formatting traits, `Send`,
//! `IntoFuture`: the library marks them `#[rustc_diagnostic_item]`) is found by its path in core
//! ([`Mark::CorePath`], [`find`]).

use blaze_id::ItemKey as LibKey;
use blaze_typeck::LangItem;

use super::index::Index;
use crate::resolve::{Kind, Ns};

/// The lang items that are traits: what `TraitDef::lang` may be.
pub const TRAITS: [LangItem; 53] = [
    LangItem::Sized,
    LangItem::MetaSized,
    LangItem::PointeeSized,
    LangItem::Copy,
    LangItem::Clone,
    LangItem::Deref,
    LangItem::DerefMut,
    LangItem::Add,
    LangItem::Sub,
    LangItem::Mul,
    LangItem::Div,
    LangItem::Rem,
    LangItem::BitAnd,
    LangItem::BitOr,
    LangItem::BitXor,
    LangItem::Shl,
    LangItem::Shr,
    LangItem::AddAssign,
    LangItem::SubAssign,
    LangItem::MulAssign,
    LangItem::DivAssign,
    LangItem::RemAssign,
    LangItem::BitAndAssign,
    LangItem::BitOrAssign,
    LangItem::BitXorAssign,
    LangItem::ShlAssign,
    LangItem::ShrAssign,
    LangItem::Neg,
    LangItem::Not,
    LangItem::PartialEq,
    LangItem::PartialOrd,
    LangItem::Index,
    LangItem::IndexMut,
    LangItem::FnOnce,
    LangItem::FnMut,
    LangItem::Fn,
    LangItem::AsyncFnOnce,
    LangItem::AsyncFnMut,
    LangItem::AsyncFn,
    LangItem::Drop,
    LangItem::Sync,
    LangItem::Unpin,
    LangItem::Destruct,
    LangItem::Tuple,
    LangItem::Unsize,
    LangItem::CoerceUnsized,
    LangItem::DispatchFromDyn,
    LangItem::FnPtrTrait,
    LangItem::Pointee,
    LangItem::DiscriminantKind,
    LangItem::Future,
    LangItem::Freeze,
    LangItem::From,
];

/// The trait lang item `#[lang = "name"]` declares, if `name` is one.
pub fn trait_lang(name: &str) -> Option<LangItem> {
    TRAITS.iter().copied().find(|&l| lang_name(l) == Some(name))
}

/// How the pinned library marks a checker lang item, and so how it is found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `#[lang = "name"]`: in the library's `tables.langs` ([`Index::lang`]).
    Lang(&'static str),
    /// No `#[lang]`: the pinned core marks it `#[rustc_diagnostic_item = ".."]` (fmt/mod.rs:
    /// `#[rustc_diagnostic_item = "Display"] pub trait Display`, `#[rustc_diagnostic_item =
    /// "Debug"] pub trait Debug`; marker.rs `"Send"`; future/into_future.rs `"IntoFuture"`),
    /// which the names library does not record. Found by its path from core's root
    /// (`crate::fmt::Display`), which the names library resolves from the module of core's
    /// `#[lang = "sized"]`.
    CorePath(&'static [&'static str]),
    /// Either `#[lang = "name"]` (e.g. `#![no_core]` tests) or `core::<path>` (real `core`).
    LangOrCorePath(&'static str, &'static [&'static str]),
}

/// The `#[lang]` name of `item`, if the library marks it so ([`mark`]).
pub fn lang_name(item: LangItem) -> Option<&'static str> {
    match mark(item) {
        Mark::Lang(n) | Mark::LangOrCorePath(n, _) => Some(n),
        Mark::CorePath(_) => None,
    }
}

/// The lang trait and the method name of `which` (`FnOnce::call_once`): the one map the run's
/// builder world (run.rs `FrontAdts`) and both constant evaluator worlds (`ConstsWorld`,
/// run.rs `FrontConsts`) answer `fn_trait_method` by, so all three name one item (rule 30).
pub fn fn_trait_method(which: blaze_build::FnTrait) -> (LangItem, &'static str) {
    match which {
        blaze_build::FnTrait::Fn => (LangItem::Fn, "call"),
        blaze_build::FnTrait::FnMut => (LangItem::FnMut, "call_mut"),
        blaze_build::FnTrait::FnOnce => (LangItem::FnOnce, "call_once"),
    }
}

/// Why a lang item has no answer: a gap, never the program's "no such lang item".
pub type NoAnswer = &'static str;

/// The library item for `item`, for the items the checker asks with [`World::lang_item`]:
/// `Ok(None)` when the program's crates hold none (a `no_core` program).
///
/// [`World::lang_item`]: blaze_typeck::World::lang_item
pub fn find<'n, S: super::source::NamesSource<'n>>(index: &Index<'_>, resolver: crate::resolve::Paths<S>, item: LangItem) -> Result<Option<LibKey>, NoAnswer> {
    let path = match mark(item) {
        Mark::Lang(n) => return Ok(index.lang(n)),
        Mark::CorePath(p) => p,
        Mark::LangOrCorePath(n, p) => {
            if let Some(found) = index.lang(n) {
                return Ok(Some(found));
            }
            let Some(sized) = index.lang("sized") else { return Ok(None) };
            return Ok(in_core(resolver, sized, p).ok().flatten());
        }
    };
    let Some(sized) = index.lang("sized") else { return Ok(None) };
    in_core(resolver, sized, path)
}

/// The `Future` trait for an `async fn`'s head (the Reference, "Async functions": its return
/// type is `impl Future<Output = T>`), found from core's `Sized` alone: the head store holds
/// `Sized` ([`super::heads::Store::sized`]) and no lang table. Core declares
/// `#[lang = "future_trait"] pub trait Future` at `core::future::Future`; this is the same
/// item [`find`] gives for [`LangItem::Future`], reached by its path through the names library.
pub fn future_trait<'n, S: super::source::NamesSource<'n>>(resolver: crate::resolve::Paths<S>, sized: LibKey) -> Result<Option<LibKey>, NoAnswer> {
    in_core(resolver, sized, &["future", "Future"])
}

/// The trait at `path` in the crate of `sized` (core), resolved by the names library.
fn in_core<'n, S: super::source::NamesSource<'n>>(resolver: crate::resolve::Paths<S>, sized: LibKey, path: &[&str]) -> Result<Option<LibKey>, NoAnswer> {
    let module = resolver.names.home_of(sized).ok_or("core's `Sized` is in no module the names library knows")?;
    let mut texts: [&str; 4] = ["crate"; 4];
    let n = path.len() + 1;
    if n > texts.len() {
        return Err("its path in core is longer than three segments");
    }
    texts[1..n].copy_from_slice(path);
    let found = resolver.path(module, false, &texts[..n], Ns::Type).map_err(|_| "its path in core does not resolve")?;
    if found.consumed as usize != n || found.kind != Kind::Trait {
        return Err("its path in core names no trait");
    }
    Ok(Some(found.key))
}

/// How the pinned library marks `item`: one arm per [`LangItem`], no wildcard, so a new
/// variant does not compile until it is given its mark.
pub fn mark(item: LangItem) -> Mark {
    Mark::Lang(match item {
        LangItem::Sized => "sized",
        LangItem::MetaSized => "meta_sized",
        LangItem::PointeeSized => "pointee_sized",
        LangItem::Copy => "copy",
        LangItem::Clone => "clone",
        LangItem::Deref => "deref",
        LangItem::DerefMut => "deref_mut",
        LangItem::Add => "add",
        LangItem::Sub => "sub",
        LangItem::Mul => "mul",
        LangItem::Div => "div",
        LangItem::Rem => "rem",
        LangItem::BitAnd => "bitand",
        LangItem::BitOr => "bitor",
        LangItem::BitXor => "bitxor",
        LangItem::Shl => "shl",
        LangItem::Shr => "shr",
        LangItem::AddAssign => "add_assign",
        LangItem::SubAssign => "sub_assign",
        LangItem::MulAssign => "mul_assign",
        LangItem::DivAssign => "div_assign",
        LangItem::RemAssign => "rem_assign",
        LangItem::BitAndAssign => "bitand_assign",
        LangItem::BitOrAssign => "bitor_assign",
        LangItem::BitXorAssign => "bitxor_assign",
        LangItem::ShlAssign => "shl_assign",
        LangItem::ShrAssign => "shr_assign",
        LangItem::Neg => "neg",
        LangItem::Not => "not",
        LangItem::PartialEq => "eq",
        LangItem::PartialOrd => "partial_ord",
        LangItem::Index => "index",
        LangItem::IndexMut => "index_mut",
        LangItem::FnOnce => "fn_once",
        LangItem::FnMut => "fn_mut",
        LangItem::Fn => "fn",
        LangItem::Drop => "drop",
        LangItem::Sync => "sync",
        LangItem::Unpin => "unpin",
        LangItem::Destruct => "destruct",
        LangItem::Tuple => "tuple_trait",
        LangItem::Unsize => "unsize",
        LangItem::CoerceUnsized => "coerce_unsized",
        LangItem::DispatchFromDyn => "dispatch_from_dyn",
        LangItem::FnPtrTrait => "fn_ptr_trait",
        LangItem::FnOnceOutput => "fn_once_output",
        LangItem::Pointee => "pointee_trait",
        LangItem::Metadata => "metadata_type",
        LangItem::DynMetadata => "dyn_metadata",
        LangItem::DiscriminantKind => "discriminant_kind",
        LangItem::Discriminant => "discriminant_type",
        LangItem::PhantomData => "phantom_data",
        LangItem::OwnedBox => "owned_box",
        LangItem::CStr => "CStr",
        LangItem::ManuallyDrop => "manually_drop",
        LangItem::Future => "future_trait",
        LangItem::FormatArguments => "format_arguments",
        LangItem::PanicBoundsCheck => "panic_bounds_check",
        LangItem::IntoIterIntoIter => "into_iter",
        LangItem::IteratorNext => "next",
        LangItem::OptionSome => "Some",
        LangItem::OptionNone => "None",
        LangItem::TryTraitBranch => "branch",
        LangItem::TryTraitFromResidual => "from_residual",
        LangItem::TryTraitFromOutput => "from_output",
        LangItem::ControlFlowContinue => "Continue",
        LangItem::ControlFlowBreak => "Break",
        LangItem::Range => "Range",
        LangItem::RangeFrom => "RangeFrom",
        LangItem::RangeTo => "RangeTo",
        LangItem::RangeFull => "RangeFull",
        LangItem::RangeInclusiveNew => "range_inclusive_new",
        LangItem::RangeToInclusive => "RangeToInclusive",
        LangItem::IntoFutureIntoFuture => "into_future",
        LangItem::FuturePoll => "poll",
        LangItem::PinNewUnchecked => "new_unchecked",
        LangItem::GetContext => "get_context",
        LangItem::PollReady => "Ready",
        LangItem::PollPending => "Pending",
        LangItem::FutureOutput => "future_output",
        LangItem::Freeze => "freeze",
        LangItem::AsyncFn => "async_fn",
        LangItem::AsyncFnMut => "async_fn_mut",
        LangItem::AsyncFnOnce => "async_fn_once",
        LangItem::AsyncFnOnceOutput => "async_fn_once_output",
        LangItem::CallOnceFuture => "call_once_future",
        LangItem::CallRefFuture => "call_ref_future",
        // `IntoFuture` the trait has no `#[lang]`; its fn is `into_future`.
        LangItem::Send => return Mark::CorePath(&["marker", "Send"]),
        LangItem::IntoFuture => return Mark::CorePath(&["future", "IntoFuture"]),
        LangItem::FmtDisplay => return Mark::CorePath(&["fmt", "Display"]),
        LangItem::FmtDebug => return Mark::CorePath(&["fmt", "Debug"]),
        LangItem::FmtLowerHex => return Mark::CorePath(&["fmt", "LowerHex"]),
        LangItem::FmtUpperHex => return Mark::CorePath(&["fmt", "UpperHex"]),
        LangItem::FmtOctal => return Mark::CorePath(&["fmt", "Octal"]),
        LangItem::FmtBinary => return Mark::CorePath(&["fmt", "Binary"]),
        LangItem::FmtLowerExp => return Mark::CorePath(&["fmt", "LowerExp"]),
        LangItem::FmtUpperExp => return Mark::CorePath(&["fmt", "UpperExp"]),
        LangItem::FmtPointer => return Mark::CorePath(&["fmt", "Pointer"]),
        LangItem::From => return Mark::LangOrCorePath("from", &["convert", "From"]),
    })
}

/// Every [`LangItem`] in declaration order, for the test that each has an answer in the pinned
/// library (tests/front2_bounds.rs, which checks this list's length against
/// `core::mem::variant_count::<LangItem>()` and its order, so a new variant cannot be left out).
/// The list's one home is blaze-typeck's `LangItem::ALL` (the checked-body record decodes a
/// lang item by it).
pub const ALL: [LangItem; 99] = LangItem::ALL;
