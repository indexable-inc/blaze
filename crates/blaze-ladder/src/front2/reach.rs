//! Every fn reachable from a checked body, each lowered, converted and checked in its own env:
//! a free fn or a member of a trait or an impl by [`lower_made`]. The walk is a
//! work list over the checker's callees (no recursion), capped by the names it walks
//! ([`cap_of`]); each fn's
//! outcome is kept, a failure by its cause's class, so the census is one line and a grouped
//! list. A call is walked to the body it runs ([`callee_target`]): a trait's method on a concrete
//! receiver to the selected impl's method; on a parameter or `dyn`, nothing at the site.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_lower::scoped::Prepares;
use blaze_typeck::{Checked, ItemKey};

use super::body::{Body, BodyError};
use super::lower_made::{LowerTarget, lower_made};
use super::check::{BodyScope, CheckFail, Records, Target, callee_target, check_body};
use super::check::{make_all, world_at, MOST_ROUNDS};
use super::keys::checker_of;
use super::langs::lang_name;
use blaze_surface::ItemDetail;
use super::heads::HeadError;
use super::keys::{MemberKind, Origin};
use blaze_typeck as tc;
use super::types::Why;
use crate::class_chain::class_chain;
use super::spell::Spelling;
use crate::front::FnLang;
use super::back_needs::BackNeeds;

mod initializers;
mod instances;

use self::instances::Instances;

/// The most fns one walk visits (rule 18: a table has a cap), sized by need: a walk reaches
/// only fns of the program's crates, and the toolchain's core alone holds 29,243 fn heads
/// (port27/front2/heads-4.txt: 10,979 fns and 18,264 impl fns), alloc and std together fewer
/// than as many again, bstd a few thousand: 65,536 is above the whole library's fn count, so
/// the cap is met only by a walk that does not end (a bug). The hello world's walk reached
/// 1,093 from `main` (list-AE); the entry program's reach was refused at the old 4,096.
/// [`Census::queued`] says how many were still waiting when the cap stopped it.
///
/// Since 2026-11-17 this bounds ROUNDS only (an ADT row's, a lazy fill's): the walk's own cap
/// is sized by the program's names at its start ([`cap_of`]), as a 1,084-crate program
/// (tempo) holds far more fns than the toolchain's library.
pub const MOST_FNS: usize = 65_536;

/// The walk's cap per item its names hold ([`items_held`]): the walk tables each fn item, ctor
/// and variant once, and beside them the fns declared in bodies (keys.rs `Keys::local`), which
/// the names' tables do not list; twice the items is above both together.
pub const NEED_FACTOR: usize = 2;

/// The absolute ceiling of a walk's table, whatever the names hold (rule 18).
pub const MOST_FNS_CEILING: usize = 1 << 23;

/// How many fns a [`Census`] lists by path when the cap stopped it ([`capped_report`]).
const MOST_WAITING: usize = 20;
/// How many crates and repeated keys [`capped_report`] lists.
const MOST_CRATES: usize = 15;
const MOST_REPEATS: usize = 10;

/// The class of a fn outside [`Within::crates`]: tabled from its head only, never lowered,
/// checked or walked.
pub const OUTSIDE: &str = "outside: not walked (its crate is not one the walk is limited to)";

/// What a walk is limited to: the crates whose fns it lowers and checks (`None`: all), and a
/// cap the caller sets in place of the one sized by need (`None`: [`cap_of`]).
#[derive(Clone, Copy, Debug)]
pub struct Within<'a> {
    pub crates: Option<&'a [usize]>,
    pub most_fns: Option<usize>,
}

impl Within<'static> {
    /// Every crate, the cap by need.
    pub const ALL: Within<'static> = Within { crates: None, most_fns: None };
}

/// The items, members and enum variants `names` holds over every crate: the most distinct
/// fns and ctors a walk over them can table but for body-declared fns ([`NEED_FACTOR`]). Read
/// from each crate's tables' lengths (`Tables::texts`, `members`, `enums`): no item is read.
pub fn items_held(names: &blaze_scope::resolve::Names<'_>) -> usize {
    let mut n = 0usize;
    for c in names.crates() {
        n += c.tables.texts.len();
        n += c.tables.members.values().map(|m| m.len()).sum::<usize>();
        n += c.tables.enums.values().map(|v| v.len()).sum::<usize>();
    }
    n
}

/// The walk's cap over names holding `items` ([`items_held`]): [`NEED_FACTOR`] times them,
/// under [`MOST_FNS_CEILING`]. A walk past it tables more fns than exist: it does not end.
pub fn cap_of(items: usize) -> usize {
    items.saturating_mul(NEED_FACTOR).clamp(1, MOST_FNS_CEILING)
}

/// The crate of checker key `k` (a member's is its container's), as `r`'s names place it.
pub(crate) fn key_crate(r: &Records<'_, '_>, k: ItemKey) -> Option<usize> {
    let lib = match r.keys.origin(k) {
        Some(Origin::Member { container, .. }) => r.keys.library_in(r.names, *container)?,
        _ => r.keys.library_in(r.names, k)?,
    };
    r.names.crate_of(lib)
}

/// The fns of `fns` by crate (`usize::MAX`: no crate known), most first, then by crate.
pub(crate) fn by_crate(r: &Records<'_, '_>, fns: &[(ItemKey, Reached)]) -> Vec<(usize, usize), Global> {
    let mut crates: Vec<usize, Global> = Vec::with_capacity_in(fns.len(), Global);
    crates.extend(fns.iter().map(|(k, _)| key_crate(r, *k).unwrap_or(usize::MAX)));
    crates.sort_unstable();
    let mut out: Vec<(usize, usize), Global> = Vec::new_in(Global);
    for c in crates {
        match out.last_mut() {
            Some((last, n)) if *last == c => *n += 1,
            _ => out.push((c, 1)),
        }
    }
    out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// A crate's name for a report.
fn crate_shown(r: &Records<'_, '_>, c: usize) -> String {
    match r.names.crates().get(c) {
        Some(k) => String::from(k.name),
        None => String::from("(no crate)"),
    }
}

/// What a capped walk met, bounded: the [`MOST_CRATES`] crates with the most fns tabled, the
/// [`MOST_REPEATS`] keys tabled more than once, and [`MOST_WAITING`] of the fns still queued by
/// path: enough to tell a big program from an instance that repeats.
pub(crate) fn capped_report(r: &Records<'_, '_>, census: &Census<'_>) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let crates = by_crate(r, &census.fns);
    let _ = writeln!(out, "REACHED BY CRATE ({} fns tabled in {} crates; the {} with the most)", census.fns.len(), crates.len(), crates.len().min(MOST_CRATES));
    for &(c, n) in crates.iter().take(MOST_CRATES) {
        let _ = writeln!(out, "  {n} {}", crate_shown(r, c));
    }
    let mut keys: Vec<ItemKey, Global> = Vec::with_capacity_in(census.fns.len(), Global);
    keys.extend(census.fns.iter().map(|(k, _)| *k));
    keys.sort_unstable();
    let mut repeats: Vec<(usize, ItemKey), Global> = Vec::new_in(Global);
    let mut i = 0;
    while i < keys.len() {
        let j = keys[i..].iter().position(|k| *k != keys[i]).map_or(keys.len(), |n| i + n);
        if j - i > 1 {
            repeats.push((j - i, keys[i]));
        }
        i = j;
    }
    repeats.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let _ = writeln!(out, "TABLED MORE THAN ONCE: {} keys", repeats.len());
    for &(n, k) in repeats.iter().take(MOST_REPEATS) {
        let _ = writeln!(out, "  {n} {}", fn_shown(r, k));
    }
    let _ = writeln!(out, "STILL QUEUED ({} waiting; {} shown, the next to be read first)", census.queued, census.waiting.len());
    for &k in census.waiting.iter() {
        let _ = writeln!(out, "  {}", fn_shown(r, k));
    }
    out
}


/// What became of one reachable fn.
#[derive(Debug)]
pub enum Reached {
    /// Lowered, converted and checked with no gap.
    Checked,
    /// No body: a required trait method (its impl is picked by instance resolution, which
    /// this walk does not do), a foreign fn or an intrinsic; counted apart, never as checked.
    NoBody,
    /// A constructor: the language's own fn, nothing to walk.
    Ctor,
    /// A trait method on a type parameter: walked where the instance is made.
    AtInstance,
    /// A trait method on `dyn Trait`: walked where the object is made.
    AtObject,
    /// Stopped: the cause's class (for grouping), the whole cause, and the typed refusal
    /// itself where the site has one (a check's `CheckFail`, a lowering's `BodyError`: a
    /// survey's left-out row keeps it, program/left_out.rs); `None` at a site that names its
    /// cause by text only (a key clash, a spelling, a lang fn the builder needs, ..).
    Failed { class: Box<str>, detail: Box<str>, refusal: Option<Box<super::program::Front2Refusal>> },
}

/// The walk's outcome: every fn reached, in the order first reached.
pub struct Census<'n> {
    pub fns: Vec<(ItemKey, Reached), Global>,
    /// Each [`Reached::Checked`] fn's lowering and checked body, in the order checked: what a
    /// program's table is made of, so no body is lowered or checked twice (rule 77).
    pub bodies: Vec<(Body<'n>, Checked<'static>), Global>,
    /// The state recipe of every async block of each checked body (coroutine_recipes.rs), made
    /// with its check from the same lowering and record (rule 77).
    pub recipes: Vec<super::coroutine_recipes::RecipeRow, Global>,
    /// Whether the cap ([`Census::cap`]) stopped the walk.
    pub capped: bool,
    /// The fns still waiting in the work list when the cap stopped it (0 when it ended).
    pub queued: usize,
    /// The walk's cap: [`cap_of`] the names' [`Census::items`], or the caller's
    /// ([`Within::most_fns`], then [`Census::forced`]).
    pub cap: usize,
    /// The items, members and variants the names held at the walk's start ([`items_held`]).
    pub items: usize,
    pub forced: bool,
    /// When capped: the next [`MOST_WAITING`] fns the walk would have read, the next first.
    pub waiting: Vec<ItemKey, Global>,
    /// The calls of bodies that did not check which only their types could name (a method
    /// call, a path through a type, a trait's fn, a qualified path): not followed.
    pub not_followed: usize,
    /// Each step the walk took, as (the fn whose reading queued it, the fn queued), in the
    /// order queued; a fn the builder calls on its own account (FnLang::ALL, drop glue's
    /// `Drop::drop`, an overloaded index, a held future's `poll`) is queued from [`AMBIENT`],
    /// and an impl fn an instance resolves to from the trait's fn decided at the call. What a
    /// test's static reach is read from (program/suite.rs `Suite::reach_of`, test.md piece 13).
    pub edges: Vec<(ItemKey, ItemKey), Global>,
    /// The `(trait fn, self shape)` pairs the instance questions asked, sorted
    /// (reach/instances.rs): exactly the pairs the walk met, never every trait fn times every
    /// shape.
    pub instance_pairs: Vec<(ItemKey, tc::Shape), Global>,
}

/// The source of every edge the walk takes for the builder's own account (no body names the
/// fn it queues): in every entry's reach. No item has the checker key 0 (a library key's first
/// 8 bytes are its crate's prefix, a content hash).
pub const AMBIENT: ItemKey = ItemKey(0);

/// One entry a walk starts from: `first`, checked as `owner` at `scope`.
pub struct Start<'a, 'c, 'n, 's> {
    pub owner: ItemKey,
    pub first: First<'a, 'c, 'n>,
    pub scope: &'a BodyScope<'s>,
}

/// Files each fn queued, and each outcome filed, since `from` was read as an edge from it.
fn note_edges(edges: &mut Vec<(ItemKey, ItemKey), Global>, from: ItemKey, queued: &[(ItemKey, Option<Reached>)], filed: &[(ItemKey, Reached)]) {
    for &(to, _) in queued {
        if to != from {
            edges.push((from, to));
        }
    }
    for (to, _) in filed {
        if *to != from {
            edges.push((from, *to));
        }
    }
}

/// Where a walk starts: a body that checked, or one that did not, whose calls by name are
/// still followed ([`queue_by_name`]) so the walk is total.
pub enum First<'a, 'c, 'n> {
    /// A body that checked, without its lowering: what the builder calls for a
    /// `format_args!` or an overloaded index it writes is not read ([`BackNeeds`]).
    Checked(&'a mut Checked<'c>),
    /// A body that checked, with its lowering: read whole, as every body the walk checks.
    CheckedBody(&'a Body<'n>, &'a mut Checked<'c>),
    Unchecked(&'a Body<'n>),
}

/// What `body` (written at `scope`, it did not check) reaches BY NAME, read from its lowering:
/// each path naming a free fn or an impl's fn, whole, is queued (a call by name resolves
/// without the body's types); each call only its types could name (a method call, a path
/// through a type or a qualified path, a trait's fn, which needs `Self`) is counted in
/// `not_followed`. The records a path's resolution needs are made as the checker makes them.
fn queue_by_name(r: &mut Records<'_, '_>, body: &Body<'_>, scope: &BodyScope<'_>, queue: &mut Vec<(ItemKey, Option<Reached>), Global>, not_followed: &mut usize) {
    // Names only: the body's expressions, paths and segments, never a type (the records are
    // made between rounds, so no view of `r.store.tys` is held across them). A type read
    // through this view would be refused as past an empty table, never answered wrong.
    let view = body.typeck.view(tc::PTys::EMPTY);
    let mut found: Vec<ItemKey, Global> = Vec::new_in(Global);
    let mut names: Vec<tc::Name, Global> = Vec::new_in(Global);
    let mut by_type = 0;
    for _ in 0..MOST_ROUNDS {
        found.clear();
        by_type = view.exprs.iter().filter(|e| matches!(e, tc::Expr::MethodCall { .. })).count();
        let world = world_at(r, scope);
        for (at, bp) in view.paths.iter().enumerate() {
            if bp.anchor.is_some() {
                by_type += 1;
                continue;
            }
            let def = match bp.lang {
                // A lang path (`?`'s `Try::branch`, a panic fn): its fn by the `#[lang]` name.
                Some(l) => match lang_name(l).and_then(|n| r.index.lang(n)) {
                    Some(lib) if world.resolver().member_of(lib).is_some() => {
                        by_type += 1;
                        None
                    }
                    Some(lib) if matches!(world.resolver().detail(lib), Some(ItemDetail::Fn { .. })) => Some(tc::Def::Fn(checker_of(lib))),
                    _ => None,
                },
                None => {
                    names.clear();
                    let (start, len) = (bp.segments.start as usize, bp.segments.len as usize);
                    let Some(segs) = view.segments.get(start..start + len) else { continue };
                    names.extend(segs.iter().map(|s| s.name));
                    match world.path(bp.scope, tc::PathIdx(at as u32), &names) {
                        Ok(Ok(p)) if p.consumed as usize == names.len() => Some(p.def),
                        // `Type::f`: the fn is found on the type's impls, by its type.
                        Ok(Ok(_)) => {
                            by_type += 1;
                            None
                        }
                        _ => None,
                    }
                }
            };
            if let Some(tc::Def::Fn(k)) = def {
                found.push(k);
            }
        }
        let fills = world.fills.into_inner();
        drop(world.gaps.into_inner());
        if fills.is_empty() || make_all(r, fills, body.owner).is_err() {
            break;
        }
    }
    for k in found {
        let of_trait = matches!(r.keys.origin(k), Some(Origin::Member { container, kind: MemberKind::Fn, .. }) if !r.store.impls.contains_key(container));
        if of_trait {
            by_type += 1;
        } else {
            queue.push((k, None));
        }
    }
    *not_followed += by_type;
}

/// The class of a cause: its `Debug` form up to the first field.
fn class_of(debug: &str) -> Box<str> {
    let end = debug.find(['(', ' ', '{']).unwrap_or(debug.len());
    Box::from(&debug[..end])
}

fn body_failed(e: &BodyError) -> Reached {
    Reached::Failed { class: format!("lower: {}", lower_class(e)).into_boxed_str(), detail: format!("{e}").into_boxed_str(), refusal: None }
}

/// What refused a body's lowering, as a class that groups (rule 19): the error's variant and,
/// for `Lowered` and `Convert`, its first finding's cause (a macro's host ask, a construct's
/// syntax kind and why, a diagnostic's kind; the refusal's variant and the type builder's). The
/// census counts by it; `blaze check`'s "not checked because lowering refused (..)" line names
/// it, with the macro's path for a macro call ([`lower_refusal`]).
pub fn lower_class(e: &BodyError) -> String {
    let d = format!("{e:?}");
    // `Lowered` and `Convert` are many causes under one variant: the class names the first
    // finding's cause (a macro's host ask, a construct's syntax kind, a diagnostic's kind; the
    // refusal's variant and the type builder's), so the census counts each cause apart.
    let cause = match e {
        BodyError::Lowered { findings, .. } => match findings.first() {
            Some(super::body::BodyFinding::Macro { cause, .. }) => format!(": Macro {cause}"),
            Some(super::body::BodyFinding::Unhandled { syntax, why, .. }) => format!(": Unhandled {syntax:?} {}", class_chain(&format!("{why:?}"), 3)),
            Some(super::body::BodyFinding::Diag { kind, .. }) => format!(": Diag {}", class_of(&format!("{kind:?}"))),
            Some(super::body::BodyFinding::CfgOut) => String::from(": CfgOut"),
            None => String::new(),
        },
        // A macro's own error at each call: counted as the program's errors, by the first.
        BodyError::Calls { errors, .. } => match errors.first() {
            Some(e) => format!(": {} errors at macro calls, first `{}!`: {}", errors.len(), e.path, e.why),
            None => String::new(),
        },
        BodyError::Convert { refused, why, .. } => {
            format!(": {}{}", class_chain(&format!("{refused:?}"), 2), why.as_ref().map(|w| format!(" {}", class_chain(&format!("{w:?}"), 4))).unwrap_or_default())
        }
        // A head the body needs: the head error's variant and, for an item's types, why
        // (`Head: Item Unsupported("..")`), never `Head` alone (port27/mini/clap-root).
        BodyError::Head { error, .. } => match error {
            HeadError::Item(_, why) => format!(": Item {}", class_chain(&format!("{why:?}"), 3)),
            other => format!(": {}", class_of(&format!("{other:?}"))),
        },
        _ => String::new(),
    };
    format!("{}{cause}", class_of(&d))
}

/// [`lower_class`], with the path of the macro whose call the lowering could not take when
/// that is the first finding: what a reader of `blaze check`'s note acts on (`Lowered: Macro
/// Failed(Expand(Host ProcMacro` alone names no macro).
pub fn lower_refusal(e: &BodyError) -> String {
    let class = lower_class(e);
    match e {
        BodyError::Lowered { findings, .. } => match findings.first() {
            // An expansion's own error (an unset `env!` variable, `compile_error!`, no rule
            // matched) also says what it names (blaze-lower by_name.rs `Failure::Diagnostic`:
            // the variable, the message): "`env!` failed" alone does not say which (rule 19).
            Some(super::body::BodyFinding::Macro { path, why, cause, .. }) if cause.starts_with("Failed(Diagnostic") => format!("{class}, the call of `{path}!`: {why}"),
            Some(super::body::BodyFinding::Macro { path, .. }) => format!("{class}, the call of `{path}!`"),
            _ => class,
        },
        BodyError::Calls { .. } => format!("{class} (errors of the program, not a refusal)"),
        _ => class,
    }
}

/// `out` (a [`Reached::Failed`] made from `refusal`'s text) carrying `refusal` itself.
fn with_refusal(mut out: Reached, refusal: super::program::Front2Refusal) -> Reached {
    if let Reached::Failed { refusal: slot, .. } = &mut out {
        *slot = Some(Box::new(refusal));
    }
    out
}

fn check_failed(r: &Records<'_, '_>, e: &CheckFail) -> Reached {
    let (class, detail): (String, String) = match e {
        // The keys named: the caller, the callee and the item the resolution stopped at.
        CheckFail::Resolve { owner, item, error, args } => {
            let at = match error {
                tc::ResolveError::NotGround { item: stopped } => format!(" (stopped at `{}`)", fn_shown(r, *stopped)),
                _ => String::new(),
            };
            let shown = format!("in `{}`, the call of `{}` at [{args}]: {error}{at}", fn_shown(r, *owner), fn_shown(r, *item));
            (format!("check: {}", class_of(&format!("{e:?}"))), shown)
        }
        CheckFail::Gap { gaps, .. } => {
            let first = gaps.first().map(|g| format!("{g:?}")).unwrap_or_default();
            (format!("check gap: {}", class_of(&first)), format!("{} gaps; the first: {}", gaps.len(), gaps.first().map(|g| super::row_path::gap_shown(r, g)).unwrap_or_default()))
        }
        // By the head error's variant chain and, for a path that did not resolve, the path as
        // written (from the item's text): head failures of unrelated causes are not one class
        // (walk-AH Brief 2's addendum: 117 "check: Head").
        CheckFail::Head { owner, error } => {
            let class = match error {
                HeadError::Item(lib, why) => {
                    let chain = class_chain(&format!("{why:?}"), 3);
                    let path = match why {
                        Why::Resolve(_, at) => r.names.item_text(*lib).and_then(|t| t.text.get(at.start as usize..at.end as usize)),
                        _ => None,
                    };
                    match path {
                        Some(path) => {
                            let cut = path.char_indices().nth(80).map_or(path, |(i, _)| &path[..i]);
                            format!("check: Head: Item: {chain}: `{cut}`")
                        }
                        None => format!("check: Head: Item: {chain}"),
                    }
                }
                other => format!("check: Head: {}", class_chain(&format!("{other:?}"), 3)),
            };
            (class, super::row_path::head_fail_shown(r, *owner, error))
        }
        CheckFail::Check { error, .. } => (format!("check: Check: {}", class_chain(&format!("{error:?}"), 3)), format!("{e}")),
        // By the first diagnostic's code and its message's first words (`E0277 Param(0):
        // MetaSized`): bodies with diagnostics of unrelated causes are not one class.
        CheckFail::Diags { diags, .. } => {
            let class = match diags.first() {
                Some((code, _, about)) => {
                    let mut words = String::new();
                    for word in about.split_whitespace().take(4) {
                        if !words.is_empty() {
                            words.push(' ');
                        }
                        words.push_str(word);
                    }
                    let cut = words.char_indices().nth(60).map_or(&*words, |(i, _)| &words[..i]);
                    format!("check: Diags: {} {cut}", code.code())
                }
                None => String::from("check: Diags"),
            };
            (class, format!("{e}"))
        }
        other => {
            let d = format!("{other:?}");
            (format!("check: {}", class_of(&d)), format!("{other}"))
        }
    };
    Reached::Failed { class: class.into_boxed_str(), detail: detail.into_boxed_str(), refusal: None }
}

/// A fn's name for messages: a member's as `container::name`.
pub(crate) fn fn_shown<'n, S: super::source::NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> String {
    super::row_path::shown_name(r.names, &*r.keys, &*r.store, k)
}

/// A body that finished with diagnostics, shown so a reader can judge each: the fn's name (a
/// member's as `container::name`), its text's first line, and the text each diagnostic covers.
fn diags_failed(r: &Records<'_, '_>, k: ItemKey, e: &CheckFail) -> Reached {
    let mut out = check_failed(r, e);
    let Reached::Failed { detail, .. } = &mut out else { return out };
    let diags: &[(_, _, _)] = match e {
        CheckFail::Diags { diags, .. } => diags,
        _ => &[],
    };
    let name = fn_shown(r, k);
    let text = match r.keys.library_in(r.names, k) {
        Some(lib) => r.names.item_text(lib).map(|t| t.text),
        None => r.store.fns.get(&k).map(|h| h.text),
    };
    let first = text.and_then(|t| t.lines().next()).unwrap_or("?");
    let mut d = format!("{name} (`{first}`): {detail}");
    for (_, at, _) in diags {
        let covered = text.and_then(|t| t.get(at.start as usize..at.end as usize)).unwrap_or("?");
        let cut = covered.char_indices().nth(240).map_or(covered, |(i, _)| &covered[..i]);
        d.push_str(&format!("\n    text {}..{}: {cut}", at.start, at.end));
    }
    *detail = d.into_boxed_str();
    out
}

/// Queues what each call in `c` (checked as `owner`, written at `scope`) runs: a body to walk,
/// or an outcome decided at the site (a ctor; a trait method on a parameter or on `dyn`; a
/// failed selection, kept under the callee's key).
fn queue_targets(r: &mut Records<'_, '_>, scope: &BodyScope<'_>, owner: ItemKey, c: &mut Checked<'_>, queue: &mut Vec<(ItemKey, Option<Reached>), Global>) {
    for i in 0..c.callees.len() {
        let item = c.callees.column::<1>()[i];
        let args = c.callees.column::<2>()[i];
        // A builtin `clone` at a closure of this body: its captures' `Copy` asked here, where
        // records are made (check.rs `reach_capture_copy`).
        let target = callee_target(r, scope, owner, item, args, &mut c.types).and_then(|t| match t {
            Target::AtInstance(_) => super::check::reach_capture_copy(r, scope, owner, c, args).map(|()| t),
            t => Ok(t),
        });
        queue.push(match target {
            Ok(Target::Body(k)) => (k, None),
            Ok(Target::Ctor(k)) => (k, Some(Reached::Ctor)),
            Ok(Target::AtInstance(k)) => (k, Some(Reached::AtInstance)),
            Ok(Target::AtObject(k)) => (k, Some(Reached::AtObject)),
            Err(e) => {
                let out = check_failed(r, &e);
                (item, Some(with_refusal(out, super::program::Front2Refusal::Check(e))))
            }
        });
    }
}

/// Walks from `first`'s callees (`first` checked as `owner` at `scope`) and from each fn the
/// back half calls on its own account ([`FnLang::ALL`], the seam's one list) that the
/// program's world declares: each fn once, its own callees queued when it checks.
/// A body that did not check (`first` unchecked, or one met on the way) is no dead end: what it
/// calls by name is walked too ([`queue_by_name`]), so the walk visits every fn reachable.
pub fn reach<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, owner: ItemKey, first: First<'_, '_, '_>, scope: &BodyScope<'_>) -> Census<'n> {
    let mut starts = Vec::with_capacity_in(1, Global);
    starts.push(Start { owner, first, scope });
    reach_all(r, prepared, starts, &[], Within::ALL)
}

/// [`reach`] from MANY entries at once (a test suite's, program/suite.rs): one walk, so a fn two
/// entries reach is lowered and checked once (rule 77); [`Census::edges`] keeps which entry
/// reached what. `also` are roots with no body read here (a test's `Termination::report` fn,
/// test_entry.rs), walked like any queued fn. `within` limits the crates walked (a fn of
/// another is filed as [`OUTSIDE`], from its head only) and may set the cap.
pub fn reach_all<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, starts: Vec<Start<'_, '_, '_, '_>, Global>, also: &[ItemKey], within: Within<'_>) -> Census<'n> {
    let items = items_held(r.names);
    let cap = within.most_fns.unwrap_or_else(|| cap_of(items));
    let mut seen: HashMap<ItemKey, (), Global> = HashMap::new_in(Global);
    // Each entry is walked already: a call back to one is no new fn.
    for s in starts.iter() {
        seen.insert(s.owner, ());
    }
    let mut queue: Vec<(ItemKey, Option<Reached>), Global> = Vec::new_in(Global);
    for l in FnLang::ALL {
        let Some(lib) = r.index.fn_lang(l) else { continue };
        queue.push(match r.keys.item(lib) {
            Ok(k) => (k, None),
            Err(clash) => (clash.key, Some(Reached::Failed { class: Box::from("key clash"), detail: format!("{clash}").into_boxed_str(), refusal: None })),
        });
    }
    let mut census = Census { fns: Vec::new_in(Global), bodies: Vec::new_in(Global), recipes: Vec::new_in(Global), capped: false, queued: 0, cap, items, forced: within.most_fns.is_some(), waiting: Vec::new_in(Global), not_followed: 0, edges: Vec::new_in(Global), instance_pairs: Vec::new_in(Global) };
    note_edges(&mut census.edges, AMBIENT, &queue, &[]);
    queue.extend(also.iter().map(|&k| (k, None)));
    // What the builder calls on its own for what the reached bodies write, queued as read.
    let mut needs = BackNeeds::new();
    // The statics and constants reached bodies name: their initializers read once each.
    let mut inits = initializers::Initializers::new();
    let mut firsts: Vec<&Checked<'_>, Global> = Vec::with_capacity_in(starts.len(), Global);
    for s in starts {
        let (queue_mark, fns_mark) = (queue.len(), census.fns.len());
        match s.first {
            First::Checked(c) => {
                queue_targets(r, s.scope, s.owner, c, &mut queue);
                needs.read(r, s.owner, None, c, &mut queue, &mut census.fns);
                firsts.push(c);
            }
            First::CheckedBody(body, c) => {
                queue_targets(r, s.scope, s.owner, c, &mut queue);
                needs.read(r, s.owner, Some(body), c, &mut queue, &mut census.fns);
                firsts.push(c);
            }
            First::Unchecked(body) => {
                queue_by_name(r, body, s.scope, &mut queue, &mut census.not_followed);
            }
        }
        note_edges(&mut census.edges, s.owner, &queue[queue_mark..], &census.fns[fns_mark..]);
    }
    let mut instances = Instances::default();
    // The fn read last, with the queue's and the census's lengths when its reading began: what
    // was queued or filed since is its edges, noted before the next pop.
    let mut reading: Option<(ItemKey, usize, usize)> = None;
    // Rounds: the work list drained, then the impl fns the back half's instances can resolve
    // to, until a round adds none (each round adds a fn, so the cap bounds the rounds).
    loop {
    while let Some((k, decided)) = {
        if let Some((from, q, f)) = reading.take() {
            note_edges(&mut census.edges, from, &queue[q.min(queue.len())..], &census.fns[f.min(census.fns.len())..]);
        }
        queue.pop()
    } {
        // A trait's provided fn met on a parameter or `dyn` is decided at the site; it is walked
        // as a body only when an instance resolves to it (an impl leaves it out: `Instances`),
        // and its decided outcome is then replaced by the walk's. The site's decision is kept
        // apart first (`Instances::note_decided`): neither that replacement nor `seen` below
        // may stop the fn's instances from being asked.
        instances.note_decided(k, decided.as_ref());
        let provided = decided.is_none() && instances.declared.remove(&k).is_some();
        if provided {
            census.fns.retain(|(f, _)| *f != k);
        } else if seen.insert(k, ()).is_some() {
            continue;
        }
        // From here every fn queued and outcome filed until the next pop is `k`'s reading.
        reading = Some((k, queue.len(), census.fns.len()));
        if matches!(decided, Some(Reached::AtInstance | Reached::AtObject)) {
            instances.declared.insert(k, ());
        }
        if census.fns.len() >= census.cap {
            census.capped = true;
            census.queued = queue.len() + 1;
            census.waiting.push(k);
            census.waiting.extend(queue.iter().rev().take(MOST_WAITING - 1).map(|(q, _)| *q));
            break;
        }
        if let Some(outcome) = decided {
            census.fns.push((k, outcome));
            continue;
        }
        // A fn of a crate the walk is not limited to: its head is the store's (made where a
        // body that calls it was checked); it is neither lowered nor walked.
        if let Some(crates) = within.crates
            && !key_crate(r, k).is_some_and(|c| crates.contains(&c))
        {
            let detail = format!("`{}` is of a crate outside the walk's ({} crates named)", fn_shown(r, k), crates.len());
            census.fns.push((k, Reached::Failed { class: Box::from(OUTSIDE), detail: detail.into_boxed_str(), refusal: None }));
            continue;
        }
        // A fn whose head says it has no body (a foreign fn's declaration, an intrinsic's):
        // nothing to lower; what calls it is the table's to say.
        if r.store.fns.get(&k).is_some_and(|h| !h.has_body) {
            // The declarations that are calls of Rust fns: alloc's `extern "Rust"` allocator
            // entries run their defaults (std's `__rdl_*`, [`super::alloc_shim`]); core's
            // `#[lang = "panic_impl"]` runs the program's `#[panic_handler]` fn
            // ([`super::panic_entry`]); each is queued here as its callee; every other fn with
            // no body is `NoBody`.
            // Every other `extern "Rust"` `#[rustc_std_internal_symbol]` declaration runs its
            // one definition, the panic runtime's by the strategy ([`super::internal_symbol`]).
            // The panic runtime's entries bind by the program's strategy, read from its inputs
            // (crate 0's cfg: crate::panic_strategy::of_program). Only a declaration the
            // runtime binds asks it; there an unreadable strategy is refused by name.
            let alloc = r.store.tys.alloc();
            let (outcome, callee) = match super::alloc_shim::reached(r, k, alloc) {
                Some(bound) => bound,
                None => match crate::panic_strategy::of_program(r.names) {
                    Ok(panic) => match super::internal_symbol::reached(r, k, panic, alloc) {
                        Some(bound) => bound,
                        None => super::panic_entry::reached(r, k, alloc),
                    },
                    Err(why) if super::internal_symbol::declares(r.names, &*r.store, k, alloc).is_ok_and(|d| d.is_some()) => {
                        (Reached::Failed { class: why.class().into(), detail: why.to_string().into_boxed_str(), refusal: None }, None)
                    }
                    Err(_) => super::panic_entry::reached(r, k, alloc),
                },
            };
            queue.extend(callee.map(|h| (h, None)));
            census.fns.push((k, outcome));
            continue;
        }
        // A head or a constant the lowering needs is made and the body lowered again
        // (front2/lower_made.rs, the one home of that loop).
        let target = match r.keys.library_in(r.names, k) {
            Some(lib) => LowerTarget::Free(lib),
            None => LowerTarget::Member(k),
        };
        let lowered = lower_made(r, prepared, target);
        let mut body = match lowered {
            Ok(b) => b,
            Err(BodyError::NoBody(_)) => {
                census.fns.push((k, Reached::NoBody));
                continue;
            }
            Err(e) => {
                let out = body_failed(&e);
                census.fns.push((k, with_refusal(out, super::program::Front2Refusal::Lower(e))));
                continue;
            }
        };
        let alloc = r.store.tys.alloc();
        let expand_res = if body.typeck.inline_fors.is_empty() {
            Ok(())
        } else {
            super::inline_for::expand_concrete_body(r, prepared, &mut body)
        };
        let view = body.lowered.view();
        let spell = match Spelling::of(&view) {
            Ok(s) => s,
            Err(e) => {
                let d = format!("{e:?}");
                census.fns.push((k, Reached::Failed { class: Box::from("spelling"), detail: d.into_boxed_str(), refusal: None }));
                continue;
            }
        };
        let scope = body.scope(&spell);
        // Each async block's state recipe is made with the check (coroutine_recipes.rs).
        let done = expand_res.and_then(|()| check_body(r, &body, &scope)).and_then(|c| {
            super::coroutine_recipes::recipes_of(r, &body, &c, &mut census.recipes, alloc)
                .map_err(|error| CheckFail::Check { owner: body.owner, error })?;
            Ok(c)
        });
        let checked = match done {
            Ok(mut c) => {
                queue_targets(r, &scope, body.owner, &mut c, &mut queue);
                census.fns.push((k, Reached::Checked));
                needs.read(r, body.owner, Some(&body), &c, &mut queue, &mut census.fns);
                Some(c)
            }
            Err(e) => {
                let out = diags_failed(r, k, &e);
                census.fns.push((k, with_refusal(out, super::program::Front2Refusal::Check(e))));
                queue_by_name(r, &body, &scope, &mut queue, &mut census.not_followed);
                None
            }
        };
        drop(scope);
        drop(spell);
        drop(view);
        if let Some(c) = checked {
            census.bodies.push((body, c));
        }
    }
    // The last fn read before the list ran dry (or the cap stopped it).
    if let Some((from, q, f)) = reading.take() {
        note_edges(&mut census.edges, from, &queue[q.min(queue.len())..], &census.fns[f.min(census.fns.len())..]);
    }
    // What a reached static's or constant's initializer names is reached too
    // (reach/initializers.rs): its fns are walked before the instances' round.
    // An initializer that writes a closure is added to the census's bodies as its closures'
    // owner, a body like a fn's (reach/initializers.rs, the body-owner rule).
    if !census.capped && inits.round(r, prepared, &firsts, &mut census.bodies, &mut needs, &mut queue, &mut census.fns, &mut census.edges) {
        continue;
    }
    if census.capped || !instances.round(r, &firsts, &mut census, &needs, &inits.coerced, &mut queue) {
        break;
    }
    }
    census.instance_pairs = instances.into_pairs();
    census
}
