//! A written `self::` or `super::` inside a body's block names the module around the BODY, never
//! the block's module (tempo, 2026-11-16: tokio-1.53.3 src/sync/mpsc/chan.rs:306 `use
//! super::block::Read;` inside `fn recv`, serde_core src/ser/impls.rs:269 and :703 `use
//! super::SerializeStruct`, opentelemetry-otlp exporter/tonic/trace.rs:74
//! `super::handle_interceptor_error!`).
//!
//! The Reference, `paths.qualifiers.mod-self`: "`self` resolves the path relative to the current
//! module"; `paths.qualifiers.super`: "`super` in a path resolves to the parent module"; and
//! "Block expressions": a block is an anonymous scope, no module. So in a block `self` is the
//! nearest enclosing module that is no block's, `super` its parent; `pub(super)` and `pub(self)`
//! of an item in a block count from that module too (`vis.scoped`). A block's module
//! ([`crate::blocks`]) still opens with `use super::*;`, the glob that gives the block its
//! lexical scope: that one reaches the block's own parent (the block around it, or the body's
//! module), which the controls below check.
//!
//! Every answer is read twice, from the fresh solve's `Names` and from the names records'
//! `RecordNames`, and the two must be equal (as tests/prelude_records.rs does).

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_surface::{Namespace, PathKind, ScopeError, Visibility};

use super::entry::{Texts, input, resolved};
use crate::names::{PathError, Resolved, World, Written, path};
use crate::names_record::{LoadNames, RecordNames, encode};
use crate::resolve::{CrateNames, Names, aligned};

const TEXT: &str = "#![no_core]
macro_rules! m { () => {} }
pub mod block { pub enum Read { V } }
pub mod a {
    pub struct Outer;
    pub fn recv() { use super::block::Read; m!(); }
}
pub mod s {
    pub struct Sh;
    pub fn f() { struct Sh; use self::Sh as S2; m!(); }
}
pub mod n {
    pub struct InN;
    pub fn g() { struct InOuter; { use super::block::Read as R2; use InOuter as IO; use InN as IN2; m!(); } m!(); }
}
pub mod outer {
    pub mod p {
        pub fn h() { pub(super) struct PS; pub(self) struct PSelf; struct Priv; m!(); }
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

/// The one module of crate `c` made under `parent` named `name` (a block's module: the only
/// body of each module here, so its blocks' names are unique under it).
fn made_under(c: &CrateNames<'static>, parent: ItemKey, name: &str) -> ItemKey {
    let found: Vec<ItemKey> = c.made.iter().filter(|(_, m)| m.info.parent == Some(parent) && m.info.name == Some(name)).map(|(k, _)| *k).collect();
    assert_eq!(found.len(), 1, "one module `{name}` made under {parent:x}");
    found[0]
}

fn written<'p>(kind: PathKind, segments: &'p [&'static str]) -> Written<'p, &'static str> {
    Written { kind, segments }
}

/// One path read in `module`, and what its whole path must name.
struct Probe {
    what: &'static str,
    module: ItemKey,
    kind: PathKind,
    segments: &'static [&'static str],
    want: Want,
}

enum Want {
    /// This definition.
    Is(ItemKey),
    /// Some definition, not this one.
    IsNot(ItemKey),
}

/// The written `self` and `super` of a block, a nested block and `pub(super)`/`pub(self)` in a
/// block. Before the fix: `super::block::Read` (twice) is unresolved (`super` named the body's
/// module, which holds no `block`), `self::Sh` names the block's own `Sh`, and `PS`/`PSelf`/`Priv`
/// are restricted to the block's module or to `p` instead of `outer`/`p`/`p`. The controls (a
/// name of the body's module and of the outer block, read in a block through the lexical glob)
/// pass before and after.
/// Prints: "SUPER IN BLOCK: written self and super pass over blocks; the lexical glob reaches the scope around".
#[test]
fn written_self_and_super_in_a_block_name_the_module_around_the_body() {
    let g = blaze_own::testing::heap().alloc();
    let crates = [input("user", "/user/lib.rs", &[])];
    let fresh: Names<'static> = resolved(&crates, &TEXTS);
    let c = fresh.krate(0);
    let mut wrong: Vec<String> = Vec::new();

    // No import of the crate is unresolved.
    for r in c.done.values() {
        for error in r.scope.errors.iter() {
            if let ScopeError::Import { path, .. } = error {
                wrong.push(format!("unresolved import {:?} {:?}", path.kind, path.segments));
            }
        }
    }

    let record: &'static [u8] = aligned(&encode(c, g).unwrap_or_else(|e| panic!("encode: {e:?}")), super::SyncFrame::leaked());
    let keys: Vec<CrateItemKey> = std::vec![c.key];
    let read = RecordNames::new(&keys, Loaded(std::vec![record]), g);

    let root = fresh.root(0);
    let def = |module: ItemKey, w: Written<'_, &'static str>| -> ItemKey {
        let r = path(&fresh, module, Namespace::Types, w, g).unwrap_or_else(|e| panic!("{:?} {:?} in {module:x}: {e}", w.kind, w.segments));
        assert_eq!(r.consumed, w.segments.len(), "{:?} {:?} named whole", w.kind, w.segments);
        r.def.key
    };
    let plain = |segments: &'static [&'static str]| def(root, written(PathKind::Plain, segments));
    let read_enum = plain(&["block", "Read"]);
    let (a, s, n, outer, p) = (plain(&["a"]), plain(&["s"]), plain(&["n"]), plain(&["outer"]), plain(&["outer", "p"]));
    let (a_outer, s_sh, n_in) = (plain(&["a", "Outer"]), plain(&["s", "Sh"]), plain(&["n", "InN"]));

    let block_a = made_under(c, a, "__blaze_block_0");
    let block_s = made_under(c, s, "__blaze_block_0");
    let block_n = made_under(c, n, "__blaze_block_0");
    let inner_n = made_under(c, block_n, "__blaze_block_1");
    let block_p = made_under(c, p, "__blaze_block_0");
    let in_outer = def(block_n, written(PathKind::Plain, &["InOuter"]));

    let probes = [
        // tokio's shape: the import, and the same path walked by the names library.
        Probe { what: "a: use super::block::Read", module: block_a, kind: PathKind::Plain, segments: &["Read"], want: Want::Is(read_enum) },
        Probe { what: "a: super::block::Read walked", module: block_a, kind: PathKind::Super(1), segments: &["block", "Read"], want: Want::Is(read_enum) },
        // CONTROL: the body's module's name, through the block's lexical glob.
        Probe { what: "a: Outer through the glob", module: block_a, kind: PathKind::Plain, segments: &["Outer"], want: Want::Is(a_outer) },
        // `self` is `s`, not the block: its `Sh`, not the block's.
        Probe { what: "s: use self::Sh as S2", module: block_s, kind: PathKind::Plain, segments: &["S2"], want: Want::Is(s_sh) },
        Probe { what: "s: self::Sh walked", module: block_s, kind: PathKind::SelfMod, segments: &["Sh"], want: Want::Is(s_sh) },
        // CONTROL: a plain `Sh` in the block is the block's own.
        Probe { what: "s: Sh is the block's", module: block_s, kind: PathKind::Plain, segments: &["Sh"], want: Want::IsNot(s_sh) },
        // A block in a block: `super` passes over both.
        Probe { what: "n: use super::block::Read as R2", module: inner_n, kind: PathKind::Plain, segments: &["R2"], want: Want::Is(read_enum) },
        Probe { what: "n: super::block::Read walked", module: inner_n, kind: PathKind::Super(1), segments: &["block", "Read"], want: Want::Is(read_enum) },
        // CONTROL: the inner block's glob reaches the OUTER BLOCK (its immediate parent), and
        // through its glob the body's module.
        Probe { what: "n: use InOuter as IO", module: inner_n, kind: PathKind::Plain, segments: &["IO"], want: Want::Is(in_outer) },
        Probe { what: "n: use InN as IN2", module: inner_n, kind: PathKind::Plain, segments: &["IN2"], want: Want::Is(n_in) },
    ];
    for probe in probes.iter() {
        let w = written(probe.kind, probe.segments);
        let a: Result<Resolved, PathError> = path(&fresh, probe.module, Namespace::Types, w, g);
        let b: Result<Resolved, PathError> = path(&read, probe.module, Namespace::Types, w, g);
        if a != b {
            wrong.push(format!("{}: fresh {a:?}, record {b:?}", probe.what));
        }
        let got = a.ok().and_then(|r| r.whole(probe.segments.len())).map(|d| d.key);
        let fits = match probe.want {
            Want::Is(k) => got == Some(k),
            Want::IsNot(k) => got.is_some_and(|other| other != k),
        };
        if !fits {
            wrong.push(format!("{}: names {got:?}", probe.what));
        }
    }

    // `pub(super)` in `outer::p`'s body's block is `outer`'s; `pub(self)` and private are `p`'s.
    for (name, want) in [("PS", outer), ("PSelf", p), ("Priv", p)] {
        let x = World::<&'static str>::scope(&fresh, block_p).and_then(|sc| sc.get(Namespace::Types, name)).map(|b| b.vis);
        let y = World::<&'static str>::scope(&read, block_p).and_then(|sc| sc.get(Namespace::Types, name)).map(|b| b.vis);
        if x != y {
            wrong.push(format!("{name}: fresh {x:?}, record {y:?}"));
        }
        if x != Some(Visibility::Restricted(want)) {
            wrong.push(format!("{name}: visibility {x:?}, want Restricted({want:x})"));
        }
    }

    assert!(wrong.is_empty(), "{wrong:#?}");
    std::println!("SUPER IN BLOCK: written self and super pass over blocks; the lexical glob reaches the scope around");
}
