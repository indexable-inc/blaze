//! The body of a Result-returning Tokio test, after the attribute wraps it in `block_on`.
//! The fixed pattern supplies the destination array length for `Vec::try_into`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use core::future::Future;

struct Runtime;
impl Runtime {
    fn block_on<F: Future>(&self, _body: F) -> F::Output { unimplemented!() }
}
struct Report;
struct Node;
impl Node {
    fn observe(&self, _other: &Node) {}
}
struct Cluster { nodes: Vec<Node> }
async fn start_cluster() -> Result<Cluster, Report> {
    Ok(Cluster { nodes: vec![Node, Node, Node] })
}

macro_rules! async_test {
    (async fn $name:ident() -> $result:ty { $($token:tt)* }) => {
        fn $name() -> $result {
            let body = async move { $($token)* };
            Runtime.block_on(body)
        }
    };
}

async_test! {
    async fn expanded_test() -> Result<(), Report> {
        let cluster = start_cluster().await?;
        let [leader, follower, _third] = cluster.nodes.try_into().map_err(|_| Report)?;
        leader.observe(&follower);
        Ok(())
    }
}

fn make<T>() -> T { unimplemented!() }
fn unknown_length() {
    let [_, ..] = make();
}

fn main() {}
"#;

#[test]
fn exact_pattern_infers_the_async_test_conversion_target() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let toolchain = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/eb2/src/main.rs";
    let files = [(root.to_string(), SOURCE.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
    let user = UserCrate { name: "eb2", dir: "/eb2/", root, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &toolchain).expect("fixture and standard library");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut check = |name: &str| {
        let (key, text) = names.items(0)
            .find_map(|(key, _, _)| {
                let item = names.item_text(key)?;
                (item.name == Some(name)).then_some((key, item.text))
            })
            .unwrap_or_else(|| panic!("missing fixture body {name}"));
        let verdict = world.check(BodyRef::Free(key), alloc);
        assert!(
            verdict.reads.is_some() && verdict.gaps.is_empty() && verdict.undecided.is_empty() && verdict.refused.is_none(),
            "{name}: {verdict:?}"
        );
        (text, verdict)
    };

    let (_, exact) = check("expanded_test");
    assert!(exact.diagnostics.is_empty(), "conversion to [Node; 3]: {exact:?}");

    let (item, open) = check("unknown_length");
    let [error] = &open.diagnostics[..] else { panic!("a rest cannot determine the scrutinee type: {open:?}") };
    assert_eq!(error.code, "E0282");
    assert_eq!(item.get(error.start as usize..error.end as usize), Some("[_, ..]"));
}
