//! The one interface the parts of blaze after the type check ask the front end through:
//! the constant evaluator (blaze-consteval) and the code builder (blaze-build). Rule 9 of "The
//! body: one lowering, built now (D119)" (docs/v2/plan/design/backend-next.md).
//!
//! The shape is the checker's (blaze-typeck world.rs: `Question`, `World`, `Reads`):
//!
//! - one enum [`Fact`], the question, and one enum [`Answer`], one variant per question.
//!   Exhaustive: a new question is a new variant, and every world's `match` fails to compile
//!   until it answers it. There is no default method and no silent "none": an answer the world
//!   does not have is [`Answer::Unknown`] with a reason, written on purpose, and a record not
//!   made yet is [`Answer::Missing`];
//! - one trait with one method, [`Facts::ask`], sealed: only [`Reads`] can call it;
//! - [`Reads`] records each question asked and the digest of its answer, one row a question,
//!   so a built function's or an evaluated constant's key gets its read list
//!   ([`ReadList::digest`]) from the asks it made (rule 21: a key covers every input);
//! - questions at ground arguments carry them ([`Ground`], [`Instance`], [`Term`]) and are keyed
//!   by their content, so one world serves the lazy front end and the whole program.
//!
//! Why one enum pair and not a generic `ask<Q: Question>(q) -> Q::Answer`: a world is then one
//! `match` the compiler holds exhaustive, the inventory below is one table, and the recorder is
//! one function that keys and digests every question alike. The price is that an answer's
//! variant is checked when it arrives, not by type: [`Reads::ask`] refuses an answer to another
//! question ([`FactError::WrongAnswer`]) at its first ask. Typed accessors over [`Reads::ask`],
//! one per question, can be generated from the enum later without touching any world.
//!
//! The back end's own answer types (blaze-build's `ReprAttrs`, `Abi`, `ObjectTrait`,
//! `DataRecord`, blaze-consteval's `FnKind`, ..) stay in their crates and enter through
//! [`Vocab`], one implementation by their owner: each keeps one home (rule 30), and this crate
//! depends only on blaze-typeck and blaze-ids, below every crate that asks.
//!
//! # Inventory: every method of the four traits today, and the question it is
//!
//! The traits: blaze-build's `Adts` (lib.rs) and `LayoutFacts` (layout.rs), blaze-consteval's
//! `ConstWorld` (lib.rs, `: LayoutFacts`), blaze-ladder's `FrontEnd` (seam.rs). The worlds:
//! `FrontAdts` and `FrontConsts` (ladder run.rs, over any `FrontEnd`), `ConstsWorld` (ladder
//! front2/consts.rs), the front ends `Front` (old, seam.rs), `Front2Program` (front2/program.rs)
//! and `LazyProgram` (front2/program/lazy/front_end.rs), and the test worlds (blaze-build's
//! `TestAdts` and seven feature worlds, blaze-consteval's fixture `World` and `Withheld`).
//! "Default left by" names the worlds that do not write the method and so answer the default.
//!
//! | Method (trait) | Arguments, answer | Answered by | Default left by | Fact |
//! |---|---|---|---|---|
//! | `adt` (Adts, LayoutFacts, FrontEnd) | key, `Option<Adt>` | every world (required) | none | [`Fact::Adt`] |
//! | `repr` (LayoutFacts, FrontEnd) | key, `ReprAttrs` | every world (required) | none | [`Fact::Repr`] |
//! | `valid_range` (LayoutFacts) | key, `ValidRange` | every world (required) | none | [`Fact::ValidRange`] |
//! | `discriminant` (LayoutFacts) | adt, variant, `Result<Option<i128>, Missing>` | every world (required) | none | [`Fact::Discriminant`] |
//! | `discriminant` (FrontEnd) | adt, variant, `Option<i128>` | Front2Program, LazyProgram | Front | [`Fact::Discriminant`] |
//! | `explicit_discriminant` (ConstWorld) | adt, variant, `Option<ItemKey>` (its body) | every world (required) | none | [`Fact::DiscriminantBody`] |
//! | `normalize` (LayoutFacts) | trait, name, args, ground, `Result<PTy, BuildError>` | FrontAdts, FrontConsts, ConstsWorld | fixture, every blaze-build test world | [`Fact::Normalize`] |
//! | `normalize` (FrontEnd) | trait, name, args, ground, `Result<Option<PTy>, ResolveError>` | Front2Program, LazyProgram | Front | [`Fact::Normalize`] |
//! | `closure` (LayoutFacts) | owner, ordinal, `Option<ClosureFacts>` | FrontAdts | FrontConsts, ConstsWorld, fixture | [`Fact::Closure`] |
//! | `coroutine` (LayoutFacts) | owner, ordinal, `Option<CoroutineFacts>` | FrontAdts | FrontConsts, ConstsWorld, fixture | [`Fact::Coroutine`] |
//! | `coroutine_state` (LayoutFacts) | owner, ordinal, args, `Result<Option<StateShape>, Missing>` | FrontAdts | FrontConsts, ConstsWorld, fixture | [`Fact::CoroutineState`] |
//! | `coroutine_niche` (LayoutFacts) | owner, ordinal, args, `Result<Option<StateNiche>, Missing>` | none | every world | [`Fact::CoroutineState`] (one record with the shape) |
//! | `opaque_hidden` (LayoutFacts) | opaque, args, ground, `Result<PTy, BuildError>` | FrontAdts | FrontConsts, ConstsWorld, fixture | [`Fact::OpaqueHidden`] |
//! | `int_lit` (Adts, FrontEnd) | owner, expr, `Option<u128>` | FrontAdts, every test world; every front end (required) | none | [`Fact::Literal`] (`LitAt::Expr`) |
//! | `float_lit` (Adts) / (FrontEnd) | owner, expr, bits at the type / `(f64, f32)` | FrontAdts, TestAdts / Front2Program, LazyProgram | feature test worlds / Front | [`Fact::Literal`] |
//! | `str_lit`, `byte_str` (Adts), `string` (FrontEnd) | owner, expr, `&str` / `&[u8]` | FrontAdts, TestAdts; every front end | feature test worlds | [`Fact::Literal`] |
//! | `int_pat` (Adts, FrontEnd) | owner, pat, `Option<u128>` | FrontAdts, TestAdts, objects; every front end | other test worlds | [`Fact::Literal`] (`LitAt::Pat`) |
//! | `float_pat`, `str_pat` (Adts, FrontEnd) | owner, pat, bits / bytes | FrontAdts, TestAdts; Front2Program, LazyProgram | feature test worlds; Front | [`Fact::Literal`] |
//! | `lang_fn` (Adts) | `&str`, `Option<ItemKey>` | FrontAdts, every test world | none | [`Fact::Lang`] |
//! | `lang_adt` (Adts) | `LangItem`, `Option<ItemKey>` | FrontAdts, TestAdts, objects | other test worlds | [`Fact::Lang`] |
//! | `fn_lang`, `lang_item`, `lang_named`, `drop_trait` (FrontEnd) | lang, `Option<ItemKey>` | every front end; `lang_named`: Front2Program, LazyProgram | `lang_named`: Front | [`Fact::Lang`] |
//! | `manually_drop` (ConstWorld) | none, `Option<Option<ItemKey>>` | ConstsWorld, fixture | FrontConsts | [`Fact::Lang`] |
//! | `location_adt`, `fmt_argument_adt` (Adts, FrontEnd) | none, `Option<ItemKey>` | FrontAdts, TestAdts, fmt; Front2Program, LazyProgram | other test worlds; Front | [`Fact::Lang`] |
//! | `future_poll`, `async_items` (Adts) | none, `Option<ItemKey>` / `Option<AsyncItems>` | FrontAdts, the two coroutine test worlds | other test worlds | [`Fact::Lang`], [`Fact::LangMember`] |
//! | `fn_trait_method` (Adts, ConstWorld) | `FnTrait`, `Option<ItemKey>` | FrontAdts, closures; FrontConsts, ConstsWorld, fixture | other test worlds | [`Fact::LangMember`] |
//! | `deref_fn`, `index_fn` (Adts) | `Mutbl`, `Option<ItemKey>` | FrontAdts, TestAdts | feature test worlds | [`Fact::LangMember`] |
//! | `lang_trait_fn` (FrontEnd) | lang trait, name, `Option<ItemKey>` | every front end (required) | none | [`Fact::LangMember`] |
//! | `fmt_argument_new` (Adts, FrontEnd) | `FmtTrait`, `Option<ItemKey>` | FrontAdts, fmt; Front2Program, LazyProgram | other test worlds; Front | [`Fact::LangMember`] |
//! | `type_names` (Adts, ConstWorld) | none, `Option<&dyn TypeNames>` | ConstsWorld | FrontAdts, FrontConsts, fixture, every blaze-build test world | [`Fact::ItemPath`], [`Fact::NameText`] |
//! | `item_named` (FrontEnd) | key, `Option<Box<str>>` | every front end (required) | none | [`Fact::ItemPath`] |
//! | `panic` (Adts) | none, `Panic` | FrontAdts, TestAdts, async_unwind | other test worlds (abort) | [`Fact::Profile`] |
//! | `overflow_checks` (Adts) | none, `OverflowChecks` | every world (required, D101) | none | [`Fact::Profile`] |
//! | `nounwind` (Adts, FrontEnd) | key, `Option<bool>` | FrontAdts, TestAdts; Front2Program | feature test worlds; Front, LazyProgram | [`Fact::Nounwind`] |
//! | `object_trait` (Adts, FrontEnd) | trait, `Option<ObjectTrait>` | FrontAdts, objects; every front end (required) | other test worlds | [`Fact::ObjectTrait`] |
//! | `drop_impl` (Adts) | ground type, `Result<Option<ItemKey>, BuildError>` | every world (required) | none | [`Fact::DropImpl`] |
//! | `drop_impl` (ConstWorld) | ground type, `Option<Result<..>>` | ConstsWorld, fixture | FrontConsts | [`Fact::DropImpl`] |
//! | `adt_drop_impl` (FrontEnd) | fn index, drop trait, adt, `Result<Option<ItemKey>, _>` | every front end (required) | none | [`Fact::DropImpl`] |
//! | `abi` (Adts), `fn_abi` (FrontEnd) | key, `Option<Abi>` | FrontAdts, every test world; Front2Program | Front, LazyProgram (the tabled fn's) | [`Fact::Abi`] |
//! | `c_variadic` (Adts, FrontEnd) | key, `Option<u32>` | FrontAdts; Front2Program, LazyProgram | every test world; Front | [`Fact::CVariadic`] |
//! | `legacy_const_generics` (Adts, FrontEnd) | key, `&[u32]` | FrontAdts, TestAdts; Front2Program, LazyProgram | feature test worlds; Front | [`Fact::LegacyConstGenerics`] |
//! | `intrinsic` (Adts, FrontEnd) | key, `Option<Intrinsic>` | FrontAdts, TestAdts, fmt, objects; every front end (required) | other test worlds | [`Fact::Intrinsic`] |
//! | `static_item` (Adts, FrontEnd) | key, `Option<StaticItem>` | FrontAdts, TestAdts; every front end (required) | feature test worlds | [`Fact::Static`] |
//! | `const_value` (Adts) | item, args, `Option<&DataRecord>` | FrontAdts, TestAdts | feature test worlds | [`Fact::Value`] (`ConstOf::Item`) |
//! | `const_record`, `const_at` (FrontEnd) | item / need, args, a record | Front2Program, LazyProgram | Front | [`Fact::Value`] |
//! | `record` (ConstWorld) | record key, `Fetch<&ConstRecord>` | every world (required) | none | [`Fact::Value`] (the key the world derives) |
//! | `const_block_value` (Adts) | owner, block, args, `Option<&DataRecord>` | FrontAdts | every test world | [`Fact::Value`] (`ConstOf::Block`) |
//! | `promoted_value` (Adts) | owner, borrow, args, `Option<&DataRecord>` | FrontAdts, TestAdts | feature test worlds | [`Fact::Value`] (`ConstOf::Promoted`) |
//! | `const_fn_instance` (Adts) | instance key, ground, `Option<Instance>` | FrontAdts, TestAdts | feature test worlds | [`Fact::KeyedInstance`] |
//! | `format_template` (Adts, FrontEnd) | owner, expr, `Option<&[FmtPiece]>` | FrontAdts, fmt; Front2Program, LazyProgram | other test worlds; Front | [`Fact::FormatTemplate`] |
//! | `track_caller` (Adts, FrontEnd) | instance, ground / key, `Option<bool>` | FrontAdts, TestAdts; Front2Program | feature test worlds; Front, LazyProgram | [`Fact::TrackCaller`] |
//! | `caller_site` (Adts, FrontEnd) | owner, expr, `Option<CallerSite>` | FrontAdts, TestAdts; Front2Program | feature test worlds; Front, LazyProgram | [`Fact::CallerSite`] |
//! | `coroutine_recipe` (FrontEnd) | owner, ordinal, `Option<Result<&StateRecipe, Refusal>>` | Front2Program | Front, LazyProgram | [`Fact::CoroutineRecipe`] |
//! | `opaque_of` (FrontEnd) | opaque, `Option<(ItemKey, u32)>` | Front2Program, LazyProgram | Front | [`Fact::OpaqueOf`] |
//! | `ctor_of` (FrontEnd) | key, `Option<(ItemKey, u32)>` | every front end (required) | none | [`Fact::CtorOf`] |
//! | `body` (ConstWorld) | item, `Fetch<Option<Typed>>` | every world (required) | none | [`Fact::ConstBody`] |
//! | `fn_kind` (ConstWorld) | item, `FnKind` | every world (required) | none | [`Fact::FnKind`] |
//! | `code_id` (ConstWorld), `code_of` (FrontEnd) | item / fn index, `ContentId` | every world (required) | none | [`Fact::CodeId`] |
//! | `resolve` (ConstWorld) | item, args, ground, `Option<(ItemKey, List)>` | ConstsWorld | FrontConsts, fixture: the default resolves every item to ITSELF | [`Fact::Resolve`] |
//! | `resolve` (FrontEnd) | fn index, item, args, ground, `Result<(ItemKey, List), _>` | every front end (required) | none | [`Fact::Resolve`] |
//! | `selects` (FrontEnd) | fn index, trait, type, ground, `Result<bool, _>` | every front end (required) | none | [`Fact::Selects`] |
//! | `sig` (FrontEnd) | fn index, `FnSig` | every front end (required) | none | [`Fact::Sig`] |
//! | `facts` (Adts) | none, `&dyn LayoutFacts` | every Adts world (required) | none | none: an accessor, gone when `LayoutFacts` is |
//! | `planted` (Adts, test builds) | none, `Planted` | ArithAdts, OpAdts, TestAdts | others | none: a test's control, stays a test-only parameter |
//!
//! Not facts, and staying on `FrontEnd` (the driver's enumeration and its effects, which no
//! evaluated value or built body reads): `fn_count`, `function`, `entry`, `types`, `fn_index`,
//! `check_all`, `check_one`, `done`, `body_reads`, `soft_float_routines`, `rust_start`,
//! `static_records`, `evaluates_at_args`, `left_out` and `last_projection` (a refusal's
//! message). `source_position` (a `Location`'s line and column) is read only when the program
//! is placed and must never enter a read list (rule 71). The fn index `i` that `resolve`,
//! `selects` and `adt_drop_impl` take today names whose world selects; at ground arguments the
//! answer does not depend on it, so the facts take none.
//!
//! The questions, grouped: 20 of an item's text (by key), 2 of the library (by lang name), 6 of
//! a body (by owner and an id of the body), 10 at ground arguments, 1 of the build: 39, for the
//! 113 methods of the four traits (39 + 10 + 10 + 54; several are one question at different
//! arguments; 18 are no question: FrontEnd's 16 of the driver, `facts` and `planted`).

#![cfg_attr(not(test), no_std)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![forbid(unsafe_code)]
#![blaze::sans_io]

mod answer;
mod digest;
mod fact;
mod reads;
#[cfg(test)]
mod tests;

pub use answer::{Answer, Literal, Resolved, Unknown, Vocab};
pub use digest::{Digest, Digester};
pub use fact::{ConstOf, Fact, FactKind, Ground, Instance, LitAt, Term};
pub use reads::{FactError, FactKey, Facts, Read, ReadList, Reads, Seal};
