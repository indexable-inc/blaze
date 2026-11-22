//! D189, point 3: one [`MacroEffect`] per macro DEFINITION, keyed by the definition (rule 65):
//! whether an expansion of it may write something seen outside the body it is called in
//! (`emits_surface`) and whether it may bind, at the top of the calling block, a name a later
//! call's path starts with (`binds_at_top`).
//!
//! A `macro_rules!` or `macro` is read by its text
//! ([`blaze_file_items::nested::effect_through`]), each call it makes through `$crate`
//! resolved in the DEFINING crate's finished names (the Reference, "Macros by example",
//! "Hygiene": `$crate` "refers to the crate defining the macro") and joined with that
//! definition's effect, read the same way. That the names are finished is what makes the
//! answer exact: a dependency's always are, and this crate's are once its module names are
//! solved, which D189's order puts before any body is read (the surface pass runs after
//! Names). A builtin's effect is known by its name; a proc macro, a builtin derive or
//! attribute, a path that resolves to nothing, a definition no record holds, and a chain
//! deeper than [`MAX_EFFECT_DEPTH`] are [`MacroEffect::ANY`].
//!
//! The walk is sans-IO: every name and definition it reads comes through [`EffectNames`],
//! whose implementation notes each read in the asker's read list (rule 21: a finished crate
//! whose answer moves enters the asker again). [`Effects`] keeps the answers for one asker.

use blaze_file_items::nested::{MacroEffect, effect_through};
use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_surface::Namespace;

use crate::task::{MacroBody, MacroSrc};

/// The most definitions deep [`Effects::of`] follows `$crate` calls (rule 18): one reached
/// deeper is [`MacroEffect::ANY`], never [`MacroEffect::NONE`].
pub const MAX_EFFECT_DEPTH: usize = 16;

/// The finished names the walk reads, each read noted by the implementation in its asker's
/// read list.
pub trait EffectNames<'a> {
    /// The root module of the crate that defines `def` (what its `$crate` names), from that
    /// crate's finished names; `None` when no finished names hold `def`.
    fn root_of(&self, def: ItemKey) -> Option<ItemKey>;
    /// What `name` is bound to in `module`'s finished scope, in `ns`.
    fn lookup(&self, module: ItemKey, ns: Namespace, name: &'a str) -> Option<ItemKey>;
    /// Macro `def`'s source, from its crate's record.
    fn source(&self, def: ItemKey) -> Option<MacroSrc<'a>>;
}

/// A builtin function-like macro's effect, by its name (core `macros/mod.rs`): `include!`
/// parses a file as items or an expression, so it may do anything; `global_asm!` emits
/// symbols to the link; every other builtin expands to an expression, a literal or nothing.
pub fn builtin_effect(name: &str) -> MacroEffect {
    match name {
        "include" => MacroEffect::ANY,
        "global_asm" => MacroEffect { emits_surface: true, binds_at_top: false },
        _ => MacroEffect::NONE,
    }
}

/// The effects answered for one asker, by definition.
pub struct Effects<'k> {
    known: HashMap<ItemKey, MacroEffect, Alloc<'k>>,
}

impl<'k> Effects<'k> {
    pub fn new_in(alloc: Alloc<'k>) -> Effects<'k> {
        Effects { known: HashMap::new_in(alloc) }
    }

    /// How many definitions have an answer kept.
    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// `def`'s effect, `mac` its source (already read by the asker). Kept by `def`; each
    /// definition reached is kept too when its answer is whole (see [`Effects::step`]).
    /// `scratch` holds the walk's stack.
    pub fn of<'a, N: EffectNames<'a>>(&mut self, names: &N, def: ItemKey, mac: MacroSrc<'a>, scratch: Alloc<'_>) -> MacroEffect {
        if let Some(known) = self.known.get(&def) {
            return *known;
        }
        let mut open: AVec<'_, ItemKey> = AVec::new_in(scratch);
        // Every definition a cycle cut is on the walk from `def`, so `def`'s answer is whole.
        let (effect, _) = self.step(names, def, mac, &mut open);
        self.known.insert(def, effect);
        effect
    }

    /// `def`'s effect with `open` the definitions being read on the walk to it: `(effect,
    /// cut)`. A call back to one of `open` (tokio's `select!` calls `$crate::select!`) adds
    /// nothing the walk does not read already, so it answers NONE there and `cut` is set: an
    /// answer that leaned on a cut holds for this walk only and is not kept, unless it is
    /// [`MacroEffect::ANY`], which nothing can raise.
    fn step<'a, N: EffectNames<'a>>(&mut self, names: &N, def: ItemKey, mac: MacroSrc<'a>, open: &mut AVec<'_, ItemKey>) -> (MacroEffect, bool) {
        let texts: [&'a str; 2] = match mac.body {
            MacroBody::Rules(rules) => [rules, ""],
            MacroBody::Macro2 { params, body } => [params.unwrap_or(""), body],
            MacroBody::BuiltinFn(name) => return (builtin_effect(name), false),
            // D189: "A proc macro is `emits_surface`"; what it binds is not known either.
            MacroBody::BuiltinDerive { .. } | MacroBody::BuiltinOther(_) | MacroBody::Proc { .. } => return (MacroEffect::ANY, false),
        };
        let Some(root) = names.root_of(def) else { return (MacroEffect::ANY, false) };
        self.walk(names, def, root, texts, open)
    }

    /// The effect of a `macro_rules!` written in a body (D189, point 4: a call the body's own
    /// textual scope resolves), `rules` its rules and `root` the root module of the body's
    /// crate (what its `$crate` names). Not kept: it has no definition key; `def` names it on
    /// the walk only (a key derived from the body and the rules, unique to it).
    pub fn of_local<'a, N: EffectNames<'a>>(&mut self, names: &N, def: ItemKey, root: ItemKey, rules: &'a str, scratch: Alloc<'_>) -> MacroEffect {
        let mut open: AVec<'_, ItemKey> = AVec::new_in(scratch);
        self.walk(names, def, root, [rules, ""], &mut open).0
    }

    /// The walk of `def`'s texts, each `$crate` call resolved from `root`: `(effect, cut)` as
    /// [`Effects::step`] answers.
    fn walk<'a, N: EffectNames<'a>>(&mut self, names: &N, def: ItemKey, root: ItemKey, texts: [&'a str; 2], open: &mut AVec<'_, ItemKey>) -> (MacroEffect, bool) {
        if open.len() >= MAX_EFFECT_DEPTH {
            return (MacroEffect::ANY, false);
        }
        open.push(def);
        let mut cut = false;
        let mut callee = |segments: &[&'a str]| -> MacroEffect {
            let Some((&last, through)) = segments.split_last() else { return MacroEffect::ANY };
            let mut module = root;
            for &segment in through {
                match names.lookup(module, Namespace::Types, segment) {
                    Some(next) => module = next,
                    None => return MacroEffect::ANY,
                }
            }
            let Some(key) = names.lookup(module, Namespace::Macros, last) else { return MacroEffect::ANY };
            if open.contains(&key) {
                cut = true;
                return MacroEffect::NONE;
            }
            if let Some(known) = self.known.get(&key) {
                return *known;
            }
            let Some(src) = names.source(key) else { return MacroEffect::ANY };
            let (effect, inner_cut) = self.step(names, key, src, &mut *open);
            if !inner_cut || effect == MacroEffect::ANY {
                self.known.insert(key, effect);
            }
            cut |= inner_cut;
            effect
        };
        let mut effect = MacroEffect::NONE;
        for text in texts {
            if effect == MacroEffect::ANY {
                break;
            }
            effect = effect.or(effect_through(text, &mut callee));
        }
        open.pop();
        (effect, cut)
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use blaze_file_items::nested::MacroEffect;
    use blaze_id::{ContentId, ItemKey};
    use blaze_surface::Namespace;

    use super::{EffectNames, Effects};
    use crate::task::{MacroBody, MacroSrc};

    fn key(name: &str) -> ItemKey {
        ItemKey(ContentId::of("a macro_effect test key", name.as_bytes()))
    }

    /// Two crates' finished names as lists: `dep` (root `dep`) and `user` (root `user`). Every
    /// read is counted, so a test sees the walk ask.
    struct Lists {
        /// (module, namespace, name, what it binds).
        names: Vec<(ItemKey, Namespace, &'static str, ItemKey)>,
        /// (definition, its crate's root, its source).
        defs: Vec<(ItemKey, ItemKey, MacroSrc<'static>)>,
        reads: core::cell::Cell<usize>,
    }

    impl EffectNames<'static> for Lists {
        fn root_of(&self, def: ItemKey) -> Option<ItemKey> {
            self.defs.iter().find(|d| d.0 == def).map(|d| d.1)
        }
        fn lookup(&self, module: ItemKey, ns: Namespace, name: &'static str) -> Option<ItemKey> {
            self.reads.set(self.reads.get() + 1);
            self.names.iter().find(|n| n.0 == module && n.1 == ns && n.2 == name).map(|n| n.3)
        }
        fn source(&self, def: ItemKey) -> Option<MacroSrc<'static>> {
            self.reads.set(self.reads.get() + 1);
            self.defs.iter().find(|d| d.0 == def).map(|d| d.2)
        }
    }

    fn rules(name: &'static str, text: &'static str) -> MacroSrc<'static> {
        MacroSrc { name, body: MacroBody::Rules(text), module: key("dep"), export: true, local_inner: false }
    }

    /// The `dep` crate: `format!`-shaped (one `$crate` call of `inner!`, an expression),
    /// `emits_impl!` (its `$crate` callee writes an `impl`), `binds!` (writes a
    /// `macro_rules!` at the block's top), `again!` (calls itself through `$crate`, as tokio's
    /// `select!` does), and a proc macro `derived`.
    fn dep() -> Lists {
        let root = key("dep");
        let export = key("dep::__export");
        let mut names = Vec::new();
        let mut defs = Vec::new();
        let mut def = |name: &'static str, src: MacroSrc<'static>, at: ItemKey, as_name: &'static str| {
            names.push((at, Namespace::Macros, as_name, key(name)));
            defs.push((key(name), root, src));
        };
        def("format", rules("format", "($($a:tt)*) => { $crate::__export::inner!($($a)*) }"), root, "format");
        def("inner", rules("inner", "($($a:tt)*) => { 0 }"), export, "inner");
        def("emits_impl", rules("emits_impl", "() => { $crate::__export::writes!() }"), root, "emits_impl");
        def("writes", rules("writes", "() => { { impl $crate::Marker for u8 {} 0 } }"), export, "writes");
        def("binds", rules("binds", "() => { macro_rules! local { () => {} } }"), root, "binds");
        def("again", rules("again", "(@ $($a:tt)*) => { 0 }; ($($a:tt)*) => { $crate::again!(@ $($a)*) }"), root, "again");
        let derived = MacroSrc { name: "derived", body: MacroBody::Proc { kind: blaze_surface::ProcKind::FnLike, helpers: "" }, module: root, export: true, local_inner: false };
        def("derived", derived, root, "derived");
        names.push((root, Namespace::Types, "__export", export));
        Lists { names, defs, reads: core::cell::Cell::new(0) }
    }

    fn effect(lists: &Lists, effects: &mut Effects<'_>, name: &str) -> MacroEffect {
        let def = key(name);
        let src = lists.defs.iter().find(|d| d.0 == def).map(|d| d.2).expect("a listed definition");
        let heap = blaze_own::testing::heap();
        effects.of(lists, def, src, heap.alloc())
    }

    /// D189, point 3: a `format!`-shaped definition is NONE (no surface), read through its
    /// `$crate` call of an expression macro; the twin `emits_impl!`, whose `$crate` callee
    /// writes an `impl`, emits surface; `binds!` binds at top and emits nothing; a proc macro
    /// is ANY; a self-call through `$crate` is cut and adds nothing. The planted control: the
    /// same `format!` with its `$crate` callee unresolvable (the `__export` module missing) is
    /// ANY, so the NONE is the walk's answer and not the text's. Each answer is kept: asking
    /// again reads nothing. Prints nothing.
    #[test]
    fn a_definition_s_effect_follows_its_crate_calls() {
        let heap = blaze_own::testing::heap();
        let lists = dep();
        let mut effects = Effects::new_in(heap.alloc());
        assert_eq!(effect(&lists, &mut effects, "format"), MacroEffect::NONE, "format!-shaped: no surface");
        assert_eq!(effect(&lists, &mut effects, "emits_impl"), MacroEffect { emits_surface: true, binds_at_top: false }, "its callee writes an impl");
        assert_eq!(effect(&lists, &mut effects, "binds"), MacroEffect { emits_surface: false, binds_at_top: true }, "a macro_rules! at the top");
        assert_eq!(effect(&lists, &mut effects, "derived"), MacroEffect::ANY, "a proc macro");
        assert_eq!(effect(&lists, &mut effects, "again"), MacroEffect::NONE, "a self-call adds nothing");
        let reads = lists.reads.get();
        assert_eq!(effect(&lists, &mut effects, "format"), MacroEffect::NONE);
        assert_eq!(lists.reads.get(), reads, "a kept answer reads nothing");
        let mut cut = dep();
        cut.names.retain(|n| n.2 != "__export");
        let mut fresh = Effects::new_in(heap.alloc());
        assert_eq!(effect(&cut, &mut fresh, "format"), MacroEffect::ANY, "the control: an unresolved `$crate` call may do anything");
    }
}
