//! Tests are programs: a test is an entry function of a program, and its result is a record
//! ([`crate::record::TestRecord`]) keyed by what it can observe.
//!
//! **The key** ([`test_key`]): the entry, its reach ([`reach`]: every function instance it can
//! call, transitively, by instance key, so what each took in is in it; every static it can
//! read, by the key of its initial value; and the span of every item a reached panic location
//! is in, as panic messages print lines), and its arguments. The outside reads it made (files,
//! environment) are kept in the record and checked when it is found
//! ([`crate::record::TestRecord::holds`]). Only tests whose key moved, or whose outside reads
//! read differently, run again.
//!
//! A static's value reaches a test two ways: the test reads it, and the functions its value
//! points at (a vtable, a function pointer) are callable. Both are in the reach; a reach that
//! leaves statics out misses an edit of a static's value
//! ([`crate::Plant::ReachWithoutStatics`], which the tests catch).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::BTreeSet;

use crate::Plant;
use crate::key::{InstanceKey, ItemId, Node, Symbol};
use crate::program::{Program, SpanRow};
use crate::record::{TestKey, ValueKey};
use crate::codec::Hasher;

/// The domain of a test's key.
pub const TEST_DOMAIN: &str = "blaze program test 1";

/// What a test can observe of its program, each list sorted.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Reach {
    pub functions: Vec<(Symbol, InstanceKey), Global>,
    pub statics: Vec<(Symbol, ValueKey), Global>,
    pub spans: Vec<(ItemId, SpanRow), Global>,
}

/// The reach of `entry` in `program`: from the manifest's rows alone.
pub fn reach(program: &Program, entry: &Symbol, plant: Plant) -> Reach {
    let mut seen = BTreeSet::new_in(Global);
    let mut stack = Vec::new_in(Global);
    seen.insert(Node::Function(*entry));
    stack.push(Node::Function(*entry));
    let mut out = Reach::default();
    let mut items = BTreeSet::new_in(Global);
    let with_statics = plant != Plant::ReachWithoutStatics;
    while let Some(node) = stack.pop() {
        let (calls, statics) = match node {
            Node::Function(symbol) => {
                let Some(row) = program.functions().get(&symbol) else { continue };
                out.functions.push((symbol, row.key));
                for item in &row.loc_items {
                    items.insert(*item);
                }
                (&row.calls, &row.statics)
            }
            Node::Static(symbol) => {
                let Some(row) = program.statics().get(&symbol) else { continue };
                out.statics.push((symbol, row.key));
                (&row.calls, &row.statics)
            }
        };
        for callee in calls {
            if seen.insert(Node::Function(*callee)) {
                stack.push(Node::Function(*callee));
            }
        }
        if with_statics {
            for target in statics {
                if seen.insert(Node::Static(*target)) {
                    stack.push(Node::Static(*target));
                }
            }
        }
    }
    for item in items {
        if let Some(span) = program.spans().get(&item) {
            out.spans.push((item, span.clone()));
        }
    }
    out.functions.sort_unstable();
    out.statics.sort_unstable();
    out
}

/// The key of the result of test `entry` with `args`, over its `reach`.
pub fn test_key(entry: &Symbol, reach: &Reach, args: &[u8]) -> TestKey {
    let mut hasher = Hasher::new(TEST_DOMAIN);
    hasher.id(&entry.item.0).id(&entry.args.0);
    hasher.update(&(reach.functions.len() as u64).to_le_bytes());
    for (symbol, key) in &reach.functions {
        hasher.id(&symbol.item.0).id(&symbol.args.0).id(&key.0);
    }
    hasher.update(&(reach.statics.len() as u64).to_le_bytes());
    for (symbol, key) in &reach.statics {
        hasher.id(&symbol.item.0).id(&symbol.args.0).id(&key.0);
    }
    hasher.update(&(reach.spans.len() as u64).to_le_bytes());
    for (item, span) in &reach.spans {
        hasher.id(&item.0).part(&span.file).update(&span.line.to_le_bytes());
    }
    hasher.part(args);
    TestKey(hasher.finish())
}
