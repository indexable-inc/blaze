//! [`crate::names::written_module`]: the module a path head `self` or `super` names, for a
//! reader that holds no segment to walk (a glob `use self::*;` / `use super::*;`: blaze-ladder
//! front2 block_traits.rs `Reader::glob`). The same rule as a walked path's head
//! (super-in-block-1): a block's module is anonymous and passed over (the Reference,
//! `paths.qualifiers.mod-self`, `paths.qualifiers.super`, "Block expressions"). Read from the
//! fresh solve's `Names` and from the names records' `RecordNames`, equal.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::{PathError, Why, Written, path, written_module};
use crate::names_record::{LoadNames, RecordNames, encode};
use crate::resolve::{CrateNames, Names, aligned};

const TEXT: &str = "#![no_core]
macro_rules! m { () => {} }
pub mod q {
    pub mod deep {
        pub fn f() { { m!(); } m!(); }
    }
}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", TEXT)]]);

struct Loaded(Vec<&'static [u8]>);

impl LoadNames<'static> for Loaded {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        self.0.get(krate).copied()
    }
}

fn made_under(c: &CrateNames<'static>, parent: ItemKey, name: &str) -> ItemKey {
    let found: Vec<ItemKey> = c.made.iter().filter(|(_, m)| m.info.parent == Some(parent) && m.info.name == Some(name)).map(|(k, _)| *k).collect();
    assert_eq!(found.len(), 1, "one module `{name}` made under {parent:x}");
    found[0]
}

/// From `f`'s outer and inner block: `self` is `deep`, `super` is `q`, `super::super` the root,
/// one more past the root. Before the change there is no `written_module` (the ladder walked
/// `home_of` one module parent per `super`, which from a block lands on the block's parent).
/// Prints: "WRITTEN MODULE: self and super from a block pass over it, fresh = record".
#[test]
fn self_and_super_heads_from_a_block_pass_over_it() {
    let g = blaze_own::testing::heap().alloc();
    let crates = [input("user", "/user/lib.rs", &[])];
    let fresh: Names<'static> = resolved(&crates, &TEXTS);
    let c = fresh.krate(0);
    let record: &'static [u8] = aligned(&encode(c, g).unwrap_or_else(|e| panic!("encode: {e:?}")), super::SyncFrame::leaked());
    let keys: Vec<CrateItemKey> = std::vec![c.key];
    let read = RecordNames::new(&keys, Loaded(std::vec![record]), g);

    let root = fresh.root(0);
    let module = |segments: &'static [&'static str]| -> ItemKey {
        let w = Written { kind: PathKind::Plain, segments };
        path(&fresh, root, Namespace::Types, w, g).ok().and_then(|r| r.whole(segments.len())).map(|d| d.key).unwrap_or_else(|| panic!("{segments:?} from the root"))
    };
    let (q, deep) = (module(&["q"]), module(&["q", "deep"]));
    let outer = made_under(c, deep, "__blaze_block_0");
    let inner = made_under(c, outer, "__blaze_block_1");

    let mut wrong: Vec<String> = Vec::new();
    for (what, from) in [("outer block", outer), ("inner block", inner), ("deep itself", deep)] {
        for (n, want) in [(0u8, Some(deep)), (1, Some(q)), (2, Some(root)), (3, None)] {
            let a: Result<ItemKey, PathError> = written_module::<&'static str, _>(&fresh, from, n);
            let b: Result<ItemKey, PathError> = written_module::<&'static str, _>(&read, from, n);
            if a != b {
                wrong.push(format!("{what}, {n}: fresh {a:?}, record {b:?}"));
            }
            let fits = match want {
                Some(k) => a == Ok(k),
                None => a.is_err_and(|e| e.why == Why::SuperPastRoot),
            };
            if !fits {
                wrong.push(format!("{what}, {n}: {a:?}, want {want:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    std::println!("WRITTEN MODULE: self and super from a block pass over it, fresh = record");
}
