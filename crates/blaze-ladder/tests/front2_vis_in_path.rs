//! A method declared `pub(in crate::a::b)` or `pub(in super::super)` is visible exactly in the
//! module the path names and the modules under it (the Reference, "Visibility and privacy":
//! "`pub(in path)` makes an item visible within the provided path. `path` must be an ancestor
//! module of the item whose visibility is being declared").
//!
//! tempo's 32 bodies "whether the body may name the member .. of inherent impl ..: it is
//! `pub(in path)`, whose module is not resolved here" (crates/consensus/src/follow/driver/actor/
//! state.rs `pub(in crate::follow::driver) fn identity(&self)`, storage/hybrid/test/utils.rs
//! `pub(in crate::storage::hybrid) fn new()`), reduced: the head files the path's DEPTH (the
//! path names an ancestor, so no name is resolved; front2 heads.rs `MemberVis::InCrate`), and
//! the world walks the impl's module chain to it (world_items.rs `member_visible_once`).
//!
//! Oracle: `inside` (in `crate::follow::driver`) and `below` (in `driver::deeper`) call both methods and
//! are clean; `beside` (in `crate::follow`, outside both paths) calling `identity` is refused
//! E0624 (a private method), a finding and no gap. Control (rule 23): the same calls with the
//! visibility written `pub(crate)` are clean in `beside` too, so `beside`'s finding is the
//! path's.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub mod follow {
    pub mod driver {
        pub mod actor {
            pub struct State(pub u8);
            impl State {
                pub(in crate::follow::driver) fn identity(&self) -> u8 { self.0 }
                pub(in super::super) fn epoch(&self) -> u8 { self.0 }
            }
        }
        pub fn inside(s: &actor::State) -> u8 { s.identity() + s.epoch() }
        pub mod deeper {
            // Not named `deeper`: the test finds a body by its name, and the module has it too.
            pub fn below(s: &super::actor::State) -> u8 { s.identity() + s.epoch() }
        }
    }
    pub fn beside(s: &driver::actor::State) -> u8 { s.identity() }
}
fn main() {}
"#;

const CONTROL: &str = r#"
pub mod follow {
    pub mod driver {
        pub mod actor {
            pub struct State(pub u8);
            impl State {
                pub(crate) fn identity(&self) -> u8 { self.0 }
            }
        }
    }
    pub fn beside(s: &driver::actor::State) -> u8 { s.identity() }
}
fn main() {}
"#;

/// Checks each of `bodies` of the one-crate program `source`, handing each verdict to `see`.
fn check_each(source: &'static str, bodies: &[&str], see: &mut dyn FnMut(&str, &BodyVerdict<'_>)) {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/vis/src/main.rs";
    let files = [(root.to_owned(), source.to_owned())];
    let user = UserCrate {
        name: "vis", dir: "/vis/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: source }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for &name in bodies {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let v = world.check(BodyRef::Free(key), alloc);
        see(name, &v);
    }
}

#[test]
fn a_pub_in_path_method_is_visible_under_its_path_only() {
    check_each(SOURCE, &["inside", "below", "beside"], &mut |name, v| {
        assert!(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none(), "{name}: a gap or a refusal: {v:?}");
        if name == "beside" {
            let [d] = v.diagnostics.as_slice() else { panic!("ORACLE: `beside` must be refused one private method: {v:?}") };
            assert_eq!(d.code, "E0624", "ORACLE: `beside`: {d:?}");
            println!("beside: E0624 (outside `crate::follow::driver`)");
        } else {
            assert!(v.is_clean() && v.reads.is_some(), "ORACLE: `{name}` is under the path: {v:?}");
            println!("{name}: clean");
        }
    });
    check_each(CONTROL, &["beside"], &mut |name, v| {
        assert!(v.is_clean() && v.reads.is_some(), "CONTROL: `{name}` with `pub(crate)` is clean: {v:?}");
        println!("CONTROL {name}: clean with `pub(crate)`");
    });
}
