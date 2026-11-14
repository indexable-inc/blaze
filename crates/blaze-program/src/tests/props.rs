//! Hegel properties of the key rules and the record layouts.

use std::collections::{BTreeMap, BTreeSet};

use hegel::TestCase;
use hegel::generators as gs;

use crate::Plant;
use crate::inline::{InlineAttr, Keys, OwnFacts, PlanRef, PlanView, Resolution, SizeClass, inlines};
use crate::key::{ArgsKey, InstanceKey, ItemId, PlanKey, Symbol, Tier};
use crate::record::{CallerFacts, CodeHead, CodeParts, InstanceCode, LineRow, LocRow, Reloc, RelocKind, Target};
use crate::codec::{Digest, Payload};

const ATTRS: [InlineAttr; 4] = [InlineAttr::None, InlineAttr::Hint, InlineAttr::Always, InlineAttr::Never];
const COSTS: [u32; 5] = [5, 30, 45, 90, 400];

fn symbol(index: u8) -> Symbol {
    Symbol::new(ItemId(Digest::of("blaze program prop item 1", &[index])), ArgsKey::NONE)
}

fn facts(tc: &TestCase) -> OwnFacts {
    let attr = tc.draw(gs::sampled_from(ATTRS.to_vec()));
    let cost = tc.draw(gs::sampled_from(COSTS.to_vec()));
    OwnFacts { size: SizeClass::of_cost(cost), attr, cold: tc.draw(gs::booleans()) }
}

/// A call graph of plans: each symbol's plan key, facts and calls.
struct Graph(BTreeMap<Symbol, (PlanKey, OwnFacts, Vec<Symbol>)>);

impl PlanView for Graph {
    fn plan(&self, symbol: &Symbol) -> Option<PlanRef<'_>> {
        let (key, facts, calls) = self.0.get(symbol)?;
        Some(PlanRef { key: *key, facts: *facts, calls: Some(calls) })
    }
}

fn graph(tc: &TestCase) -> Graph {
    let count: u8 = tc.draw(gs::integers::<u8>().min_value(1).max_value(9));
    let mut plans = BTreeMap::new();
    for index in 0..count {
        let calls: Vec<u8> = tc.draw(gs::vecs(gs::integers::<u8>().max_value(count - 1)).max_size(4));
        let mut callees = Vec::new();
        for call in calls {
            if !callees.contains(&symbol(call)) {
                callees.push(symbol(call));
            }
        }
        let key = PlanKey(Digest::of("blaze program prop plan 1", &[index]));
        plans.insert(symbol(index), (key, facts(tc), callees));
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
#[hegel::test]
fn the_edit_tier_inlines_only_always(tc: TestCase) {
    let facts = facts(&tc);
    assert_eq!(inlines(Tier::Edit, &facts), facts.attr == InlineAttr::Always);
    if facts.attr == InlineAttr::Never {
        assert!(!inlines(Tier::Release, &facts));
    }
    if facts.attr == InlineAttr::Always {
        assert!(inlines(Tier::Release, &facts));
    }
}

/// A key is a function of the plans alone: keying the instances in any order gives the same
/// keys and the same inline trees.
#[hegel::test]
fn keys_do_not_depend_on_discovery_order(tc: TestCase) {
    let graph = graph(&tc);
    let tier = if tc.draw(gs::booleans()) { Tier::Release } else { Tier::Edit };
    let forward: Vec<Symbol> = graph.0.keys().copied().collect();
    let rotate: usize = tc.draw(gs::integers::<usize>().max_value(forward.len() - 1));
    let mut other = forward.clone();
    other.rotate_left(rotate);
    other.reverse();
    let (keys_a, a) = key_all(&graph, tier, &forward);
    let (keys_b, b) = key_all(&graph, tier, &other);
    assert_eq!(a, b);
    for symbol in &forward {
        assert_eq!(keys_a.inline_tree(symbol), keys_b.inline_tree(symbol));
    }
}

/// An inlinee's edit moves exactly its own key and the keys of the instances that took it in
/// at any depth: nothing else (the two-step rule's keys are Merkle over the inline tree).
#[hegel::test]
fn an_edit_moves_exactly_itself_and_its_inliners(tc: TestCase) {
    let mut graph = graph(&tc);
    let tier = if tc.draw(gs::booleans()) { Tier::Release } else { Tier::Edit };
    let order: Vec<Symbol> = graph.0.keys().copied().collect();
    let (keys, before) = key_all(&graph, tier, &order);
    let at: usize = tc.draw(gs::integers::<usize>().max_value(order.len() - 1));
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
}

/// A tier is in the key: the same plan at the two tiers has two keys.
#[hegel::test]
fn the_tier_is_in_the_key(tc: TestCase) {
    let graph = graph(&tc);
    let order: Vec<Symbol> = graph.0.keys().copied().collect();
    let (_, edit) = key_all(&graph, Tier::Edit, &order);
    let (_, release) = key_all(&graph, Tier::Release, &order);
    for symbol in &order {
        assert_ne!(edit[symbol], release[symbol]);
    }
}

/// A field written at a place reads back as its target, for every kind, within its range.
#[hegel::test]
fn a_relocation_reads_back_what_was_written(tc: TestCase) {
    let place: u64 = tc.draw(gs::integers::<u64>().min_value(1 << 28).max_value(1 << 40)) & !3;
    let kind = tc.draw(gs::sampled_from(vec![RelocKind::Abs64, RelocKind::Rel32, RelocKind::Arm64Branch26]));
    let words: i64 = tc.draw(gs::integers::<i64>().min_value(-(1 << 24)).max_value(1 << 24));
    let addend = match kind {
        RelocKind::Rel32 => -4,
        _ => 0,
    };
    let target = (place as i64 + 4 * words) as u64;
    let mut bytes = vec![0u8; 16];
    if kind == RelocKind::Arm64Branch26 {
        bytes[4..8].copy_from_slice(&0x9400_0000u32.to_le_bytes());
    }
    let written = kind.write(&mut bytes, 4, place, target, addend);
    assert_eq!(written, Ok(()));
    assert_eq!(kind.read(&bytes, 4, place, addend), Some(target));
    if kind == RelocKind::Arm64Branch26 {
        assert_eq!(bytes[7] & 0xfc, 0x94, "the branch's opcode is kept");
    }
}

/// A code record round-trips, and its head is readable from the record's first bytes alone.
#[hegel::test]
fn a_code_record_round_trips(tc: TestCase) {
    let words: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let mut code = Vec::new();
    for _ in 0..words {
        code.extend_from_slice(&tc.draw(gs::integers::<u64>()).to_le_bytes());
    }
    let panics: u8 = tc.draw(gs::integers::<u8>().max_value(2));
    let mut locations = Vec::new();
    for index in 0..panics {
        locations.push(LocRow { item: ItemId(Digest::of("blaze program prop loc 1", &[index])), line: u32::from(index), col: 3 });
    }
    let mut relocs = Vec::new();
    for word in 0..words {
        let pick: u8 = tc.draw(gs::integers::<u8>().max_value(4));
        let target = match pick {
            0 => continue,
            1 => Target::Function(symbol(tc.draw(gs::integers::<u8>().max_value(5)))),
            2 => Target::Static(symbol(100 + tc.draw(gs::integers::<u8>().max_value(2)))),
            3 if panics > 0 => Target::Location(0),
            _ => Target::Extern(0),
        };
        relocs.push(Reloc { at: 8 * word as u32, kind: RelocKind::Abs64, target, addend: 0 });
    }
    let parts = CodeParts {
        symbol: symbol(0),
        facts: CallerFacts { noreturn: tc.draw(gs::booleans()), nounwind: tc.draw(gs::booleans()), size: code.len() as u32 },
        code,
        unwind: tc.draw(gs::vecs(gs::integers::<u8>()).max_size(8)),
        relocs,
        locations,
        lines: vec![LineRow { offset: 0, item: ItemId(Digest::of("blaze program prop loc 1", &[9])), line: 1 }],
        externs: vec![b"blaze_panic".to_vec()],
    };
    let record = match InstanceCode::new(parts) {
        Ok(record) => record,
        Err(error) => panic!("a well-formed code record was refused: {error}"),
    };
    let bytes = record.encode();
    assert_eq!(InstanceCode::decode(&bytes), Ok(record.clone()));
    assert_eq!(CodeHead::decode_prefix(&bytes), Ok(record.head()));
    let mut calls = Vec::new();
    for reloc in record.relocs() {
        if let Target::Function(callee) = reloc.target {
            if !calls.contains(&callee) {
                calls.push(callee);
            }
        }
    }
    assert_eq!(record.calls(), &calls[..], "calls are the function fields in first-occurrence order");
}
