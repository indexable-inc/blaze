//! Properties of the key rules, each a function of one tape.

use std::collections::{BTreeMap, BTreeSet};

use blaze_sim::{Config, Tape};

use crate::Plant;
use crate::codec::Digest;
use crate::inline::{InlineAttr, Keys, OwnFacts, PlanRef, PlanView, Resolution, SizeClass, inlines};
use crate::key::{ArgsKey, InstanceKey, ItemId, PlanKey, Symbol, Tier};

const ATTRS: [InlineAttr; 4] = [InlineAttr::None, InlineAttr::Hint, InlineAttr::Always, InlineAttr::Never];
const COSTS: [u32; 5] = [5, 30, 45, 90, 400];

fn symbol(index: u8) -> Symbol {
    Symbol::new(ItemId(Digest::of("blaze program prop item 1", &[index])), ArgsKey::NONE)
}

fn facts(tape: &Tape) -> OwnFacts {
    let attr = *tape.one_of(&ATTRS, "attr");
    let cost = *tape.one_of(&COSTS, "cost");
    OwnFacts { size: SizeClass::of_cost(cost), attr, cold: tape.bool("cold") }
}

/// A call graph of plans: each symbol's plan key, facts and calls.
struct Graph(BTreeMap<Symbol, (PlanKey, OwnFacts, Vec<Symbol>)>);

impl PlanView for Graph {
    fn plan(&self, symbol: &Symbol) -> Option<PlanRef<'_>> {
        let (key, facts, calls) = self.0.get(symbol)?;
        Some(PlanRef { key: *key, facts: *facts, calls: Some(calls) })
    }
}

fn graph(tape: &Tape) -> Graph {
    let count: u8 = tape.int_in(1..=9, "count");
    let mut plans = BTreeMap::new();
    for index in 0..count {
        let calls: Vec<u8> = tape.vec(0..=4, "calls", |tape| tape.int_in(0..=count - 1, "callee"));
        let mut callees = Vec::new();
        for call in calls {
            if !callees.contains(&symbol(call)) {
                callees.push(symbol(call));
            }
        }
        let key = PlanKey(Digest::of("blaze program prop plan 1", &[index]));
        plans.insert(symbol(index), (key, facts(tape), callees));
    }
    Graph(plans)
}

fn key_all(graph: &Graph, tier: Tier, order: &[Symbol]) -> (Keys, BTreeMap<Symbol, InstanceKey>) {
    let mut keys = Keys::new(tier, Plant::None);
    let mut out = BTreeMap::new();
    for symbol in order {
        match keys.resolve(symbol, graph) {
            Resolution::Keyed(key) => {
                out.insert(*symbol, key);
            }
            Resolution::Needs(needs) => panic!("a complete graph still needs {} plans", needs.len()),
        }
    }
    (keys, out)
}

/// The fast tier takes in only what says `#[inline(always)]`; the top tier never what says
/// `#[inline(never)]`, and always what says `#[inline(always)]`.
#[test]
fn the_edit_tier_inlines_only_always() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let facts = facts(tape);
        assert_eq!(inlines(Tier::Edit, &facts), facts.attr == InlineAttr::Always);
        if facts.attr == InlineAttr::Never {
            assert!(!inlines(Tier::Release, &facts));
        }
        if facts.attr == InlineAttr::Always {
            assert!(inlines(Tier::Release, &facts));
        }
    });
}

/// A key is a function of the plans alone: keying the instances in any order gives the same
/// keys and the same inline trees.
#[test]
fn keys_do_not_depend_on_discovery_order() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let graph = graph(tape);
        let tier = if tape.bool("release tier") { Tier::Release } else { Tier::Edit };
        let forward: Vec<Symbol> = graph.0.keys().copied().collect();
        let rotate: usize = tape.int_in(0..=forward.len() - 1, "rotate");
        let mut other = forward.clone();
        other.rotate_left(rotate);
        other.reverse();
        let (keys_a, a) = key_all(&graph, tier, &forward);
        let (keys_b, b) = key_all(&graph, tier, &other);
        assert_eq!(a, b);
        for symbol in &forward {
            assert_eq!(keys_a.inline_tree(symbol), keys_b.inline_tree(symbol));
        }
    });
}

/// An inlinee's edit moves exactly its own key and the keys of the instances that took it in
/// at any depth: nothing else (the two-step rule's keys are Merkle over the inline tree).
#[test]
fn an_edit_moves_exactly_itself_and_its_inliners() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let mut graph = graph(tape);
        let tier = if tape.bool("release tier") { Tier::Release } else { Tier::Edit };
        let order: Vec<Symbol> = graph.0.keys().copied().collect();
        let (keys, before) = key_all(&graph, tier, &order);
        let at: usize = tape.int_in(0..=order.len() - 1, "edited");
        let edited = order[at];
        let mut inliners = BTreeSet::new();
        inliners.insert(edited);
        for symbol in &order {
            if keys.took_in_closure(symbol).contains(&edited) {
                inliners.insert(*symbol);
            }
        }
        if let Some(entry) = graph.0.get_mut(&edited) {
            entry.0 = PlanKey(Digest::of("blaze program prop edited plan 1", &entry.0.0.0));
        }
        let (_, after) = key_all(&graph, tier, &order);
        let moved: BTreeSet<Symbol> = order.iter().copied().filter(|symbol| before[symbol] != after[symbol]).collect();
        assert_eq!(moved, inliners);
    });
}

/// A tier is in the key: the same plan at the two tiers has two keys.
#[test]
fn the_tier_is_in_the_key() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let graph = graph(tape);
        let order: Vec<Symbol> = graph.0.keys().copied().collect();
        let (_, edit) = key_all(&graph, Tier::Edit, &order);
        let (_, release) = key_all(&graph, Tier::Release, &order);
        for symbol in &order {
            assert_ne!(edit[symbol], release[symbol]);
        }
    });
}
