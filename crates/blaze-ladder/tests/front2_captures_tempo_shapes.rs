//! Tempo's two capture groups (W4), reduced. Their first sites are from blaze-int notes
//! reports/tempo-correct-1120. Each answer is rustc's; each shape has its twin (rule 23).
//!
//! - ROLE (crates/zones/node/src/role.rs, 53 bodies, "Unsupported ClosureCaptureWitness"). A
//!   closure's structural trait (`Send`, `Sync`, `Clone`) was asked where its environment was
//!   opaque, as for a closure of another body (blaze-solve builtin.rs `closure_upvars`). Now
//!   column 3 of every decided closure is filed (blaze-ladder foreign_closures.rs `places_env`).
//!   The reduction (a GUESS at the shape role.rs reaches: a spawned `async move` whose closure
//!   reads a field through `as_ref().map(..)`, and a closure another fn returns, asked `Send`):
//!   - `role_select`: clean. TWIN `role_select_rc`: the generation holds an `Rc`: E0277.
//!   - `role_returned`: clean. TWIN `role_returned_rc`: E0277.
//! - RPC (crates/zones/node/src/rpc.rs, 34 bodies, "the coroutine witness needs the types and
//!   borrow modes of its captured places"). From rpc.rs:261, `register_async_method(name, move
//!   |params, _| { let s = cap.clone(); async move { .. s .. .await } })`, and from rpc.rs:1126,
//!   `Box::pin(async move { .. &self.eth.api .. .await })` as a `Send` future. A `move` block
//!   through `&self` captures `self` (the reference) by value: truncated at the first deref.
//!   - `rpc_register`: clean. TWIN `rpc_register_rc`: THREE E0277, as rustc reports: the
//!     closure is not `Send` (its `cap: Rc<String>`), not `Sync` (the same field), and its
//!     future is not `Send` (its `s: Rc<String>`). Three distinct predicates on two types, none
//!     implying another, so rustc's dedup keeps all three (rustc_trait_selection
//!     error_reporting/traits/mod.rs `report_fulfillment_errors`, :141-268: an error is
//!     suppressed only when another at its span implies it, `error_implies`, or covers it,
//!     `covered_by_trait_error`; ui tests trait-bounds/deep-level-send-bound-check-issue-40827
//!     and fmt/send-sync report one E0277 per failing auto-trait predicate). Read from rustc's
//!     source, not run on rustc.
//!   - `rpc_boxed`: clean. TWIN `rpc_boxed_rc`: one E0277 (the boxed future is not `Send`).
//!
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

fn spawn<F: Future + Send + 'static>(f: F) { drop(f); }
fn send<T: Send>(_: T) {}
async fn tick() {}

#[derive(Clone, Copy)]
pub enum Role { Leader, Fenced }
pub struct Generation { pub role: Role, pub name: String }
pub struct GenerationRc { pub role: Role, pub name: Rc<String> }

fn role_select(current: Option<Arc<Generation>>) {
    spawn(async move {
        tick().await;
        let active = current.as_ref().map(|generation| generation.role).unwrap_or(Role::Fenced);
        tick().await;
        let _ = (active, current);
    });
}
fn role_select_rc(current: Option<Arc<GenerationRc>>) {
    spawn(async move {
        tick().await;
        let active = current.as_ref().map(|generation| generation.role).unwrap_or(Role::Fenced);
        tick().await;
        let _ = (active, current);
    });
}

fn make(name: Arc<String>) -> impl Fn() -> usize { move || name.len() }
fn make_rc(name: Rc<String>) -> impl Fn() -> usize { move || name.len() }
fn role_returned() { send(make(Arc::new(String::new()))); }
fn role_returned_rc() { send(make_rc(Rc::new(String::new()))); }

fn register<F, Fut>(_: F)
where
    F: Fn(u8) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = usize> + Send + 'static,
{
}
fn rpc_register(seq: Arc<String>) {
    let cap = seq.clone();
    register(move |p| {
        let s = cap.clone();
        async move { tick().await; s.len() + p as usize }
    });
}
fn rpc_register_rc(seq: Rc<String>) {
    let cap = seq.clone();
    register(move |p| {
        let s = cap.clone();
        async move { tick().await; s.len() + p as usize }
    });
}

pub struct Api { pub n: u8 }
pub struct Eth { pub api: Api }
pub struct Rpc { pub eth: Eth }
pub struct ApiRc { pub n: Rc<u8> }
pub struct EthRc { pub api: ApiRc }
pub struct RpcRc { pub eth: EthRc }
async fn header(api: &Api) -> u8 { tick().await; api.n }
async fn header_rc(api: &ApiRc) -> u8 { tick().await; *api.n }
type BoxFut<'a> = Pin<Box<dyn Future<Output = u8> + Send + 'a>>;
// tempo's `fn block_by_id(&self, ..) -> BoxFut<'_>`, its receiver written as a parameter (as
// front2_witness_records.rs `boxed_field` does).
fn rpc_boxed(rpc: &Rpc) -> BoxFut<'_> { Box::pin(async move { header(&rpc.eth.api).await }) }
fn rpc_boxed_rc(rpc: &RpcRc) -> BoxFut<'_> { Box::pin(async move { header_rc(&rpc.eth.api).await }) }

fn main() {}
"#;

/// What one body's check said, kept past the checker.
#[derive(Debug)]
struct Said {
    name: &'static str,
    clean: bool,
    codes: Vec<&'static str>,
    texts: Vec<String>,
    stopped: bool,
}

/// Each free fn by name.
fn verdicts(free: &[&'static str]) -> Vec<Said> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/shapes/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "shapes", dir: "/shapes/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2021").expect("Rust 2021"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut out = Vec::new();
    for &name in free {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let v = world.check(BodyRef::Free(key), alloc);
        let said = Said {
            name,
            clean: v.is_clean() && v.reads.is_some(),
            codes: v.diagnostics.iter().map(|d| d.code).collect(),
            texts: v.diagnostics.iter().map(|d| d.text.to_string()).collect(),
            stopped: !(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none()),
        };
        // The verdicts first, every one, before any assertion.
        println!("{name}: clean {} codes {:?} stopped {} {:?}", said.clean, said.codes, said.stopped, said.texts);
        if said.stopped {
            println!("  stopped by: gaps {:?} undecided {:?} refused {:?}", v.gaps, v.undecided, v.refused);
        }
        out.push(said);
    }
    out
}

/// `clean` are accepted; each `not_send` row is a twin with the auto traits rustc reports
/// failing, one E0277 each, in any order (`Send` twice when a closure and its future both fail).
fn judge(rows: &[Said], clean: &[&str], not_send: &[(&str, &[&str])]) {
    for n in clean {
        let s = rows.iter().find(|s| s.name == *n).expect("checked above");
        assert!(s.clean && !s.stopped, "`{n}`: rustc accepts it: {s:?}");
    }
    for &(n, traits) in not_send {
        let s = rows.iter().find(|s| s.name == n).expect("checked above");
        assert!(!s.stopped, "TWIN `{n}`: a gap where rustc decides E0277: {s:?}");
        assert!(s.codes.iter().all(|c| *c == "E0277"), "TWIN `{n}`: rustc reports E0277 only: {s:?}");
        assert_eq!(s.codes.len(), traits.len(), "TWIN `{n}`: rustc reports one E0277 per failing predicate {traits:?}: {s:?}");
        for t in ["Send", "Sync"] {
            let want = traits.iter().filter(|x| **x == t).count();
            let got = s.texts.iter().filter(|x| x.lines().next().unwrap_or("").contains(t)).count();
            assert_eq!(got, want, "TWIN `{n}`: {want} E0277 naming `{t}`: {s:?}");
        }
    }
}

#[test]
fn role_shapes_have_their_closure_witness() {
    let rows = verdicts(&["role_select", "role_select_rc", "role_returned", "role_returned_rc"]);
    judge(&rows, &["role_select", "role_returned"], &[("role_select_rc", &["Send"]), ("role_returned_rc", &["Send"])]);
}

#[test]
fn rpc_shapes_have_their_coroutine_witness() {
    let rows = verdicts(&["rpc_register", "rpc_register_rc", "rpc_boxed", "rpc_boxed_rc"]);
    judge(&rows, &["rpc_register", "rpc_boxed"], &[("rpc_register_rc", &["Send", "Send", "Sync"]), ("rpc_boxed_rc", &["Send"])]);
}
