//! What a function may do, as it is declared: the one home of `#[blaze::may(..)]`
//! (docs/v2/plan/design/effects.md).
//!
//! A function's *allowance* is the set of effects it may have ([`Words`]: `panic`, `alloc`,
//! `block`, `diverge`). An effect outside it is one the function does not have, checked by the
//! effect's [`EffectDef`](crate::effects::EffectDef) (`effect_attrs::of_word`) on the one walk.
//! Where the allowance comes from ([`Marks`], [`allowed`]):
//!
//! 1. the function's own mark: `#[blaze::may(..)]`, or an old spelling (`#[blaze::nopanic]`,
//!    `#[blaze::noalloc]`, `#[blaze::nonblocking]`), which is every word but the ones it
//!    promises the absence of. It replaces what follows entirely; nothing is merged;
//! 2. else, for a method of a trait's impl (or a trait's default method), the bound: the
//!    `#[blaze::may(..)]` of the trait's method, else of the trait. An impl's own mark must stay
//!    within it ([`beyond_bound`]): an impl may declare fewer effects, never more;
//! 3. else the default: the `#![blaze::may(..)]` of the nearest module around it, else nothing
//!    at all in a crate with `#![blaze::effects]` ([`default_of`]);
//! 4. else none: the function is of a crate that asked for nothing, and may do anything.
//!
//! What the walk can't see (`Unchecked`, an unknown foreign function) may be any effect, so it
//! is refused for every word a list leaves out: only `may(panic, alloc, block, diverge)`, or a
//! callee taken at its word (`trusted = "why"`), admits it. There is no word for it.

use base_db::Db;
use hir_def::{
    AttrDefId, FunctionId, ItemContainerId, Lookup, ModuleDefId, ModuleId, ModuleIdLt, TraitId,
    attrs::{AttrFlags, EffectWord, MayAttr, MayWord, PureAttr},
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_ty::db::HirDatabase;

/// A set of [`MayWord`]s, a bit each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Words(pub u8);

impl Words {
    pub const NONE: Words = Words(0);
    /// Every word: what a function that may do anything lists.
    pub const ALL: Words = Words(0b1111);

    pub const fn one(word: MayWord) -> Words {
        Words(word.bit())
    }

    pub fn of(words: &[MayWord]) -> Words {
        Words(words.iter().fold(0, |bits, it| bits | it.bit()))
    }

    pub const fn has(self, word: MayWord) -> bool {
        self.0 & word.bit() != 0
    }

    pub const fn union(self, other: Words) -> Words {
        Words(self.0 | other.0)
    }

    pub const fn without(self, other: Words) -> Words {
        Words(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every word of it is in `other`.
    pub const fn within(self, other: Words) -> bool {
        self.0 & !other.0 == 0
    }

    /// Its words, in the order a list is written in.
    pub fn iter(self) -> impl Iterator<Item = MayWord> {
        MayWord::ALL.into_iter().filter(move |&word| self.has(word))
    }

    /// The words as a list is written: "panic, alloc".
    pub fn list(self) -> String {
        self.iter().map(MayWord::text).collect::<Vec<_>>().join(", ") // alloc: a message or a fix
    }

    /// The attribute that lists them: `#[blaze::may(panic, alloc)]`.
    pub fn attribute(self) -> String {
        format!("#[blaze::may({})]", self.list()) // alloc: a message or a fix
    }
}

/// The marks an allowance is read from, each as its set of words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Marks {
    /// The function's own mark.
    pub own: Option<Words>,
    /// The mark of the trait's method it implements, or of the trait.
    pub bound: Option<Words>,
    /// The default around it: the nearest module's, or nothing under the crate's gate.
    pub default: Option<Words>,
}

/// A bug planted for the controls of the invariant's test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Plant {
    /// No bug.
    Off,
    /// An impl's own mark is not held within its trait's.
    #[cfg(test)]
    ImplBeyondTrait,
    /// A module's default is added to a function's own mark instead of replaced by it.
    #[cfg(test)]
    DefaultMerged,
}

/// What a function with `marks` may do: its own mark, else its trait's bound, else the default
/// around it; none where nothing asks (it may do anything). The first written holds alone.
pub fn allowed(marks: &Marks) -> Option<Words> {
    allowed_with(marks, Plant::Off)
}

fn allowed_with(marks: &Marks, plant: Plant) -> Option<Words> {
    #[cfg(test)]
    {
        if plant == Plant::DefaultMerged
            && let (Some(own), Some(default)) = (marks.own, marks.default)
        {
            return Some(own.union(default));
        }
    }
    let _ = plant;
    marks.own.or(marks.bound).or(marks.default)
}

/// The words of a function's own mark that the trait method it implements does not allow.
pub fn beyond_bound(marks: &Marks) -> Words {
    beyond_with(marks, Plant::Off)
}

fn beyond_with(marks: &Marks, plant: Plant) -> Words {
    #[cfg(test)]
    {
        if plant == Plant::ImplBeyondTrait {
            return Words::NONE;
        }
    }
    let _ = plant;
    match (marks.own, marks.bound) {
        (Some(own), Some(bound)) => own.without(bound),
        _ => Words::NONE,
    }
}

/// What the check says of one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every effect it has is one it may have.
    Within,
    /// It has these effects and may not.
    Undeclared(Words),
    /// Its own mark lists these, which its trait's method does not allow.
    BeyondBound(Words),
}

/// The verdict of a function with `marks` whose effects are `inferred`.
pub fn verdict(marks: &Marks, inferred: Words) -> Verdict {
    verdict_with(marks, inferred, Plant::Off)
}

fn verdict_with(marks: &Marks, inferred: Words, plant: Plant) -> Verdict {
    let beyond = beyond_with(marks, plant);
    if !beyond.is_empty() {
        return Verdict::BeyondBound(beyond);
    }
    match allowed_with(marks, plant) {
        Some(allowed) if !inferred.within(allowed) => {
            Verdict::Undeclared(inferred.without(allowed))
        }
        _ => Verdict::Within,
    }
}

/// The default of a function from the `#![blaze::may(..)]` of the modules around it,
/// innermost first, and whether its crate has `#![blaze::effects]`: the nearest module's, else
/// nothing at all under the gate, else no default.
pub fn default_of(modules: impl IntoIterator<Item = Option<Words>>, gated: bool) -> Option<Words> {
    modules.into_iter().flatten().next().or(gated.then_some(Words::NONE))
}

/// Where a bound is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundFrom {
    /// The trait's method the function implements.
    Method(FunctionId),
    Trait(TraitId),
}

/// Where a default is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultFrom {
    Module(ModuleId),
    /// `#![blaze::effects]` at the crate's root: nothing.
    Gate,
}

/// What is declared of a function, as written and as the marks it lowers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    pub marks: Marks,
    /// Its own `#[blaze::may(..)]`, a malformed one included (which is no mark of [`Marks`]).
    pub may: Option<MayAttr>,
    /// Its own old spellings, by [`EffectWord`]'s index.
    pub negatives: [Option<PureAttr>; 3],
    pub bound_from: Option<BoundFrom>,
    pub default_from: Option<DefaultFrom>,
}

impl Declared {
    /// Whether the function has an old spelling of its own.
    pub fn has_negative(&self) -> bool {
        self.negatives.iter().any(Option::is_some)
    }

    /// The words its own old spellings promise the absence of.
    pub fn negated(&self) -> Words {
        EffectWord::ALL
            .into_iter()
            .filter(|&word| self.negatives[word as usize].is_some())
            .fold(Words::NONE, |words, word| words.union(Words::one(word.may())))
    }
}

/// A well-formed list's words.
fn words_of(may: Option<MayAttr>) -> Option<Words> {
    may.filter(|it| it.malformed.is_none()).map(|it| Words(it.words))
}

/// What is declared of `func`: the module docs say what each mark is.
pub fn declared(db: &Db, func: FunctionId) -> Declared {
    let may = AttrFlags::may_attr(db, AttrDefId::FunctionId(func));
    let negatives =
        EffectWord::ALL.map(|word| AttrFlags::effect_attr(db, AttrDefId::FunctionId(func), word));
    let mut declared =
        Declared { marks: Marks::default(), may, negatives, bound_from: None, default_from: None };
    // The function's own: its list, else every word but those an old spelling takes off.
    declared.marks.own = words_of(declared.may.clone()) // clone: a handle of an interned text
        .or_else(|| declared.has_negative().then(|| Words::ALL.without(declared.negated())));

    // The bound: the trait's method this one implements, else the trait.
    let (trait_, method) = match func.lookup(db).container {
        ItemContainerId::TraitId(trait_) => (Some(trait_), None),
        ItemContainerId::ImplId(imp) => {
            let trait_ = db.impl_trait(imp).map(|it| it.skip_binder().def_id.0);
            let name = &FunctionSignature::of(db, func).name;
            (trait_, trait_.and_then(|it| it.trait_items(db).method_by_name(name)))
        }
        _ => (None, None),
    };
    let of_method = method.and_then(|method| {
        words_of(AttrFlags::may_attr(db, AttrDefId::FunctionId(method)))
            .map(|words| (words, BoundFrom::Method(method)))
    });
    let of_trait = || {
        let trait_ = trait_?;
        words_of(AttrFlags::may_attr(db, AttrDefId::TraitId(trait_)))
            .map(|words| (words, BoundFrom::Trait(trait_)))
    };
    if let Some((words, from)) = of_method.or_else(of_trait) {
        (declared.marks.bound, declared.bound_from) = (Some(words), Some(from));
    }

    // The default: the nearest module's list, else the crate's gate.
    let Some(module) = ModuleDefId::FunctionId(func).module(db) else { return declared };
    let mut at = Some(module);
    // A module's parents end at the crate's root.
    while let Some(it) = at {
        if let Some(words) = words_of(AttrFlags::may_attr(db, AttrDefId::ModuleId(it))) {
            (declared.marks.default, declared.default_from) =
                (Some(words), Some(DefaultFrom::Module(it)));
            return declared;
        }
        at = it.containing_module(db).map(ModuleIdLt::to_static);
    }
    if gated(db, module.krate(db)) {
        (declared.marks.default, declared.default_from) =
            (default_of([], true), Some(DefaultFrom::Gate));
    }
    declared
}

/// Whether `krate` has `#![blaze::effects]` at its root: the gate the blaze edition will turn
/// on (docs/v2/plan/design/edition.md).
pub fn gated(db: &Db, krate: base_db::Crate) -> bool {
    let root = crate_def_map(db, krate).root_module_id();
    AttrFlags::query(db, AttrDefId::ModuleId(root)).contains(AttrFlags::BLAZE_EFFECTS)
}

#[cfg(test)]
mod tests {
    use hegel::{TestCase, generators as gs};

    use super::*;

    /// One function of the model: the effects its own body has, the functions it calls, and
    /// what is written for it.
    #[derive(Clone, Debug)]
    struct Function {
        own_effects: Words,
        calls: Vec<usize>,
        /// Its own list.
        own: Option<Words>,
        /// The list of the trait's method it implements.
        bound: Option<Words>,
        /// The module it is in.
        module: usize,
    }

    /// A crate of the model: modules in a tree (a module's parent has a smaller index; module
    /// 0 is the root), each with its default, and whether the crate is under the gate.
    #[derive(Clone, Debug)]
    struct Model {
        functions: Vec<Function>,
        /// Per module: its parent and its `#![blaze::may(..)]`.
        modules: Vec<(Option<usize>, Option<Words>)>,
        gated: bool,
    }

    /// The effects each function has: its own and those of everything it calls, to a fixpoint.
    fn inferred(model: &Model) -> Vec<Words> {
        let mut has: Vec<Words> = model.functions.iter().map(|it| it.own_effects).collect();
        // Each round adds a word somewhere or is the last: at most words times functions rounds.
        loop {
            let mut grew = false;
            for (at, function) in model.functions.iter().enumerate() {
                let all = function.calls.iter().fold(has[at], |all, &to| all.union(has[to]));
                grew |= all != has[at];
                has[at] = all;
            }
            if !grew {
                return has;
            }
        }
    }

    /// The model the checker is held to: whether the crate is accepted, as the design says it. A
    /// function's allowed set is its own list; else its trait method's; else the list of the
    /// nearest module around it that has one; else nothing under the gate; else anything. Its
    /// effects must be in that set, and its own list in its trait method's.
    fn spec_accepts(model: &Model) -> bool {
        let has = inferred(model);
        model.functions.iter().zip(&has).all(|(function, has)| {
            let allowed: Option<Vec<MayWord>> = match (function.own, function.bound) {
                (Some(own), _) => Some(own.iter().collect()),
                (None, Some(bound)) => Some(bound.iter().collect()),
                (None, None) => {
                    let mut module = Some(function.module);
                    let mut found = None;
                    while let Some(at) = module {
                        let (parent, default) = model.modules[at];
                        if let Some(default) = default {
                            found = Some(default.iter().collect());
                            break;
                        }
                        module = parent;
                    }
                    found.or(model.gated.then(Vec::new))
                }
            };
            let fits = allowed.is_none_or(|allowed| has.iter().all(|word| allowed.contains(&word)));
            let within_trait = match (function.own, function.bound) {
                (Some(own), Some(bound)) => own.iter().all(|word| bound.has(word)),
                _ => true,
            };
            fits && within_trait
        })
    }

    /// The checker: the marks as [`declared`] reads them ([`default_of`] over the modules
    /// around a function) and [`verdict`] of each function.
    fn checker_accepts(model: &Model, plant: Plant) -> bool {
        let has = inferred(model);
        model.functions.iter().zip(&has).all(|(function, has)| {
            let mut around = Vec::new();
            let mut module = Some(function.module);
            while let Some(at) = module {
                around.push(model.modules[at].1);
                module = model.modules[at].0;
            }
            let marks = Marks {
                own: function.own,
                bound: function.bound,
                default: default_of(around, model.gated),
            };
            verdict_with(&marks, *has, plant) == Verdict::Within
        })
    }

    fn below(tc: &TestCase, n: usize) -> usize {
        tc.draw(gs::integers::<u32>().max_value(n as u32 - 1)) as usize
    }

    fn words(tc: &TestCase) -> Words {
        Words(tc.draw(gs::integers::<u8>().max_value(Words::ALL.0)))
    }

    fn maybe_words(tc: &TestCase) -> Option<Words> {
        tc.draw(gs::booleans()).then(|| words(tc))
    }

    fn drawn(tc: &TestCase) -> Model {
        let modules: Vec<(Option<usize>, Option<Words>)> = (0..1 + below(tc, 3))
            .map(|at| ((at > 0).then(|| below(tc, at)), maybe_words(tc)))
            .collect();
        let count = 1 + below(tc, 5);
        let functions = (0..count)
            .map(|_| Function {
                // Few effects, so that some crates are accepted.
                own_effects: if below(tc, 3) == 0 { words(tc) } else { Words::NONE },
                calls: (0..below(tc, 3)).map(|_| below(tc, count)).collect(),
                own: maybe_words(tc),
                bound: if below(tc, 3) == 0 { Some(words(tc)) } else { None },
                module: below(tc, modules.len()),
            })
            .collect();
        Model { functions, modules, gated: tc.draw(gs::booleans()) }
    }

    /// The invariant: the checker accepts a crate exactly when every function's inferred
    /// effects are within its allowed set, where allowed sets flow as specified: its own mark,
    /// else its trait method's, else the nearest module's default, else nothing under the gate
    /// and anything outside it; and an impl's own mark is within its trait method's. For random
    /// small call graphs with random effect sources and random declarations.
    #[hegel::test]
    fn the_checker_accepts_exactly_what_is_within_its_allowance(tc: TestCase) {
        let model = drawn(&tc);
        assert_eq!(checker_accepts(&model, Plant::Off), spec_accepts(&model), "{model:?}");
    }

    fn function(own_effects: Words, own: Option<Words>, bound: Option<Words>) -> Function {
        Function { own_effects, calls: Vec::new(), own, bound, module: 0 }
    }

    /// Control: with an impl's own mark not held within its trait method's, the checker
    /// accepts an impl that lists `alloc` under a trait method that lists only `panic`, and
    /// the model does not.
    #[test]
    fn the_oracle_catches_an_impl_allowed_more_than_its_trait() {
        let (panic, alloc) = (Words::one(MayWord::Panic), Words::one(MayWord::Alloc));
        let model = Model {
            functions: vec![function(Words::NONE, Some(panic.union(alloc)), Some(panic))],
            modules: vec![(None, None)],
            gated: true,
        };
        assert!(!spec_accepts(&model));
        assert!(!checker_accepts(&model, Plant::Off));
        assert!(checker_accepts(&model, Plant::ImplBeyondTrait), "the plant accepts it");
    }

    /// Control: with a module's default added to a function's own mark instead of replaced by
    /// it, the checker accepts a function that panics, lists only `alloc` and is in a module
    /// whose default is `panic`, and the model does not.
    #[test]
    fn the_oracle_catches_a_module_default_merged_with_a_functions_own_mark() {
        let (panic, alloc) = (Words::one(MayWord::Panic), Words::one(MayWord::Alloc));
        let model = Model {
            functions: vec![function(panic, Some(alloc), None)],
            modules: vec![(None, Some(panic))],
            gated: false,
        };
        assert!(!spec_accepts(&model));
        assert!(!checker_accepts(&model, Plant::Off));
        assert!(checker_accepts(&model, Plant::DefaultMerged), "the plant accepts it");
    }

    /// The flow, case by case: the first mark written holds alone.
    #[test]
    fn the_first_mark_written_holds_alone() {
        let (panic, alloc) = (Words::one(MayWord::Panic), Words::one(MayWord::Alloc));
        let marks = |own, bound, default| Marks { own, bound, default };
        assert_eq!(allowed(&marks(None, None, None)), None);
        assert_eq!(allowed(&marks(None, None, Some(panic))), Some(panic));
        assert_eq!(allowed(&marks(None, Some(alloc), Some(panic))), Some(alloc));
        assert_eq!(allowed(&marks(Some(Words::NONE), Some(alloc), Some(panic))), Some(Words::NONE));
        assert_eq!(default_of([None, Some(alloc), Some(panic)], false), Some(alloc));
        assert_eq!(default_of([None, None], true), Some(Words::NONE));
        assert_eq!(default_of([None, None], false), None);
        assert_eq!(verdict(&marks(Some(alloc), None, Some(panic)), panic), Verdict::Undeclared(panic));
        assert_eq!(
            verdict(&marks(Some(panic.union(alloc)), Some(panic), None), Words::NONE),
            Verdict::BeyondBound(alloc)
        );
        assert_eq!(verdict(&marks(None, None, None), Words::ALL), Verdict::Within);
        assert_eq!(panic.union(alloc).attribute(), "#[blaze::may(panic, alloc)]");
        assert_eq!(Words::NONE.attribute(), "#[blaze::may()]");
    }
}
