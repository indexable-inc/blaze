//! An inherited bound on an associated type can become a bound on a concrete
//! marker in the caller. Its impl then determines projections in either direction.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;

const ROOT: &str = r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
mod prelude;
mod network;
mod rpc;
"#;

const NETWORK: &str = r#"
pub struct Marker;
pub struct Request;
pub struct Receipt { pub visible: bool }
pub struct Log;

pub trait Network {
    type Request;
    type Receipt;
}
impl Network for Marker {
    type Request = Request;
    type Receipt = Receipt;
}
pub trait RpcTypes {
    type Request;
    type Receipt;
    type Log;
}
impl<N: Network> RpcTypes for N {
    type Request = <N as Network>::Request;
    type Receipt = <N as Network>::Receipt;
    type Log = Log;
}
pub trait Convert { type Network: RpcTypes; }
pub trait Api {
    type Net: RpcTypes;
    type Converter: Convert<Network = Self::Net>;
}
pub trait FullApi: Api<Net: RpcTypes> {}

pub trait Primitives { type Block; }
pub struct Block;
impl Primitives for Marker { type Block = Block; }
pub trait Node { type Prims: Primitives; }
pub trait Provider: Node<Prims: Primitives> {}
"#;

const RPC: &str = r#"
use crate::network::*;

type RpcRequest<N> = <N as RpcTypes>::Request;
type RpcReceipt<N> = <N as RpcTypes>::Receipt;
type RpcLog<N> = <N as RpcTypes>::Log;
struct Batch<T>(T);
fn call<A: Api>(_: &A, _: RpcRequest<<A::Converter as Convert>::Network>) {}
fn simulate<A: Api>(_: &A, _: Batch<RpcRequest<A::Net>>) {}
fn consume_receipt(_: Receipt) {}
fn require_wrong<N: RpcTypes<Receipt = Request>>() {}

struct Zone<A>(A);
impl<A> Zone<A> where A: FullApi<Net = Marker> {
    fn request(&self, value: Request) { call(&self.0, value); }
    fn payload(&self, value: Batch<Request>) { simulate(&self.0, value); }
    fn receipt(value: Receipt) -> RpcReceipt<A::Net> { value }
    fn pass_receipt(value: RpcReceipt<A::Net>) { consume_receipt(value); }
    fn field(value: RpcReceipt<A::Net>) -> bool { value.visible }
    fn logs(value: &Batch<RpcLog<A::Net>>) -> &Batch<Log> { value }
    fn wrong_receipt() { require_wrong::<A::Net>(); }
}

struct RecoveredBlock<B>(B);
type BlockTy<N> = <<N as Node>::Prims as Primitives>::Block;
fn recovered<N: Provider<Prims = Marker>>(b: &RecoveredBlock<BlockTy<N>>) -> &RecoveredBlock<Block> { b }

// With no equality for the marker, a blanket impl may not fix the associated type.
fn unspecified<A: FullApi>() { require_wrong::<A::Net>(); }
"#;

#[test]
fn inherited_projection_bounds_use_the_concrete_impl() {
    let files = [
        SourceFile { path: "src/main.rs", text: ROOT },
        SourceFile { path: "src/prelude.rs", text: include_str!("../../blaze-cli/tests/ladder/front2/x0-clo-04-generic-fn/src/prelude.rs") },
        SourceFile { path: "src/network.rs", text: NETWORK },
        SourceFile { path: "src/rpc.rs", text: RPC },
    ];
    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "eb7", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).expect("fixture texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
    let mut world = ProjectWorld::new(&names, &frame, &files, alloc).expect("front2");
    let bodies = world.bodies(alloc);
    for name in ["request", "payload", "receipt", "pass_receipt", "field", "logs", "recovered", "wrong_receipt", "unspecified"] {
        let mut matching = bodies.iter().filter_map(|&(body, _)| {
            let ident = match body {
                BodyRef::Free(key) => names.item_text(key)?.name?,
                BodyRef::Member { name, .. } => name,
            };
            (ident == name).then_some(body)
        });
        let body = matching.next().unwrap_or_else(|| panic!("missing fixture body {name}"));
        assert!(matching.next().is_none(), "ambiguous fixture body {name}");
        let v = world.check(body, alloc);
        assert!(v.reads.is_some() && v.gaps.is_empty() && v.refused.is_none() && v.undecided.is_empty(), "{name}: {v:?}");
        if matches!(name, "wrong_receipt" | "unspecified") {
            let [d] = v.diagnostics.as_slice() else { panic!("{name}: expected one diagnostic, got {v:?}") };
            assert_eq!(d.code, "E0271", "{name}: {d:?}");
            assert!(d.text.contains("as RpcTypes>::Receipt == Request"), "{name}: {d:?}");
            if name == "wrong_receipt" {
                assert!(d.text.contains("<Marker as RpcTypes>"), "{name}: {d:?}");
            }
            println!("{name}: E0271 on RpcTypes::Receipt == Request");
        } else {
            assert!(v.diagnostics.is_empty(), "{name}: {v:?}");
            println!("{name}: clean");
        }
    }
}
