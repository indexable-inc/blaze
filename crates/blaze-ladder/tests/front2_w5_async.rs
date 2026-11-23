//! A synchronous closure can construct a future. Its await points and its locals belong
//! to the future, even when the closure itself is written inside another async body.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
struct Token { n: u8 }

async fn borrowed() {
    let n = 7u8;
    let make = || async {
        let r = &n;
        async {}.await;
        *r
    };
}

async fn owned() {
    let make = || {
        let token = Token { n: 7 };
        async move {
            let r = &token;
            async {}.await;
            r.n
        }
    };
    async {}.await;
}

async fn conflict() {
    let make = || async {
        let mut n = 7u8;
        let shared = &n;
        async {}.await;
        let unique = &mut n;
        *shared
    };
}

async fn lending() {
    let mut n = 7u8;
    let make = async || {
        async {}.await;
        n += 1;
    };
}

fn main() {}
"#;

#[test]
fn awaits_in_callback_futures_belong_to_the_future() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/w5/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "w5", dir: "/w5/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for name in ["borrowed", "owned", "conflict", "lending"] {
        let key = names.items(0).find_map(|(key, _, _)| {
            (names.item_text(key)?.name == Some(name)).then_some(key)
        }).expect("fixture body");
        let item = names.item_text(key).expect("body source").text;
        let mut flows = String::new();
        let (types, borrows) = world.check_borrows_shown(BodyRef::Free(key), &mut flows, alloc);
        assert!(types.is_clean() && types.undecided.is_empty() && types.reads.is_some(), "{name}: {types:?}");
        if name == "lending" {
            let Some(BorrowRefused::Lower(why)) = borrows.refused else { panic!("{name}: {borrows:?}") };
            assert_eq!(why.form, blaze_flow_lower::Form::AsyncClosure);
            assert!(item[why.range.start as usize..why.range.end as usize].starts_with("async ||"));
            assert!(borrows.findings.is_empty(), "{borrows:?}");
            println!("lending: refused AsyncClosure");
            continue;
        }
        assert!(borrows.refused.is_none(), "{name}: {borrows:?}");
        // Each callback future suspends once. Only owned's outer body also suspends.
        // Front2 checks the nested bodies first, then the fn and its root block.
        let (nested, root) = flows.split_once("flow of Owner:").expect("fn flow");
        let suspended: Vec<_> = nested.split("flow of ").filter(|flow| flow.contains("suspend ->")).collect();
        assert_eq!(suspended.len(), 1, "{name}: {flows}");
        assert!(suspended[0].starts_with("AsyncBlock("), "{name}: {flows}");
        assert_eq!(suspended[0].matches("suspend ->").count(), 1, "{name}: {flows}");
        assert_eq!(root.matches("suspend ->").count(), usize::from(name == "owned"), "{name}: {flows}");
        if name == "conflict" {
            let [d] = &borrows.findings[..] else { panic!("expected one live-loan conflict: {borrows:?}") };
            assert_eq!(d.code, "E0502");
            assert_eq!(&item[d.start as usize..d.end as usize], "&mut n");
            println!("conflict: E0502 at &mut n");
        } else {
            assert!(borrows.is_clean(), "{name}: {borrows:?}");
            println!("{name}: clean");
        }
    }
}
