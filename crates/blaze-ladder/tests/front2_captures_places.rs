//! Closure captures as PLACES in the borrow check (stage 2 of RFC 2229; the Reference, "Closure
//! types", "Capture modes", "Capture precision"): a Rust 2021 closure holds the places it
//! uses, so the owner's borrow check sees a loan or a move of `x.a`, not of `x`. Each case is
//! rustc's answer, each with its NEGATIVE TWIN (rule 23), reduced from tempo's 15 "a use of a
//! value after it was moved" notes (blaze-int notes reports/tempo-correct-1120/c0340wave89
//! borrow-notes.txt) where one is named:
//!
//! - H1 `move_field` (`move || drop(s.a)` then `s.b`): clean. TWIN `move_field_again`: `s.a`
//!   after the closure moved it: E0382.
//! - H2 `disjoint_mut` (`|| p.a.push(1)` held, then `&p.b`): clean. TWIN `overlap_mut`: `&p.a`
//!   while the closure holds `&mut p.a`: E0502.
//! - H3 tempo trie-shapes harness.rs:249 `trie_shape` (a field moved out, then a closure reads
//!   another by reference): clean. TWIN `trie_shape_moved`: the closure reads the moved field:
//!   E0382.
//! - H4 tempo test_utils.rs:194 `unwrap_or_else_fields` (`self.calls.unwrap_or_else(|| ..
//!   self.kind ..)`, `self` by value): clean. TWIN `unwrap_or_else_same`: the closure reads
//!   `self.calls`, moved by the receiver: E0382.
//! - H5 tempo config.rs:202 `nested_field` (`file.zone.id` moved, then a closure moving
//!   `file.zone.manifest`): clean. TWIN `nested_field_moved`: the closure reads `file.zone.id`:
//!   E0382.
//! - H6 `through_mut_ref` (`r: &mut P`, `|| r.a.push(1)` held, then `&r.b`): clean (the place is
//!   `(*r).a`). TWIN `through_mut_ref_overlap`: `&r.a`: E0502.
//! - H7 `drop_type_whole` (`D: Drop`, `move || d.a.len()` captures `d` WHOLE: a value with a
//!   destructor moves whole, `restrict_precision_for_drop_types`), then `d.b`: E0382. TWIN
//!   `no_drop_field` (the same without `Drop`): clean.
//! - H8 `union_whole` (`|| unsafe { u.a }` captures `u` whole, `restrict_precision_for_unsafe`)
//!   held, then `&mut u.b`: E0502. TWIN `struct_disjoint` (a struct): clean.
//! - H9 `shared_ref_truncated` (`r: &P`, `|| r.a.len()` captures `*r` by reference,
//!   `truncate_capture_for_optimization`; inside, `r` itself is the field's pointer) and then
//!   `&mut q.a` of another `q`: clean. TWIN `shared_ref_vs_mut`: `r: &mut P` read through
//!   `(*r).a` by the closure, then `&mut r.a`: E0502.
//!
//! - H10 `packed_borrow_whole` (`#[repr(packed)]`, `|| p.a` reads a `u32` field: a `Copy` read
//!   is a borrow, and a borrow of a packed struct's field captures the STRUCT,
//!   `restrict_repr_packed_field_ref_capture`, whatever the alignment) held, then `p.b = 1`:
//!   E0506. TWIN `unpacked_borrow_field` (no `repr(packed)`): `&q.a` only, clean. And
//!   `packed_move_field` (a BY-VALUE use, `|| take(p.s)`, keeps the field `p.s`) then
//!   `take(p.t)`: clean.
//!
//! These are the SWITCH's acceptance cases: before `blaze_typeck::ActiveEnv = Places` the
//! owner sees whole bindings and the clean cases H1-H6 and H10's report E0382, E0502 or E0506
//! (the 15 tempo notes); H7-H9 and H10's E0506 hold under both strategies and guard the
//! truncations.
//!
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub struct S { a: String, b: String }
pub struct P { a: Vec<u32>, b: Vec<u32> }
pub struct Summary { collider: Vec<u32>, accounts: Vec<u32> }
pub struct Builder { calls: Option<Vec<u32>>, kind: u32, value: String }
pub struct Zone { id: String, manifest: Option<String> }
pub struct File { zone: Zone }
pub struct D { a: String, b: String }
impl Drop for D { fn drop(&mut self) {} }
pub union U { a: u32, b: f32 }

fn take(_: String) {}
fn takev(_: Vec<u32>) {}
fn make(_: u32, _: String) -> Vec<u32> { Vec::new() }

fn move_field(s: S) { let c = move || take(s.a); c(); take(s.b); }
fn move_field_again(s: S) { let c = move || take(s.a); c(); take(s.a); }

fn disjoint_mut(mut p: P) { let mut c = || p.a.push(1); let r = &p.b; c(); let _ = r.len(); }
fn overlap_mut(mut p: P) { let mut c = || p.a.push(1); let r = &p.a; c(); let _ = r.len(); }

fn trie_shape(summary: Summary) {
    let collider = summary.collider;
    let f = || summary.accounts.len();
    let _ = f();
    takev(collider);
}
fn trie_shape_moved(summary: Summary) {
    let collider = summary.collider;
    let f = || summary.collider.len();
    let _ = f();
    takev(collider);
}

fn unwrap_or_else_fields(b: Builder) -> Vec<u32> { b.calls.unwrap_or_else(|| make(b.kind, b.value)) }
fn unwrap_or_else_same(b: Builder) -> Vec<u32> { b.calls.unwrap_or_else(|| b.calls.clone().unwrap_or_default()) }

fn nested_field(file: File) -> Option<usize> {
    let id = file.zone.id;
    take(id);
    let m = move || file.zone.manifest.map(|s| s.len());
    m()
}
fn nested_field_moved(file: File) -> Option<usize> {
    let id = file.zone.id;
    take(id);
    let m = move || Some(file.zone.id.len());
    m()
}

fn through_mut_ref(r: &mut P) { let mut c = || r.a.push(1); let b = &r.b; c(); let _ = b.len(); }
fn through_mut_ref_overlap(r: &mut P) { let mut c = || r.a.push(1); let b = &r.a; c(); let _ = b.len(); }

fn drop_type_whole(d: D) { let c = move || d.a.len(); let _ = c(); let _ = d.b.len(); }
fn no_drop_field(d: S) { let c = move || d.a.len(); let _ = c(); let _ = d.b.len(); }

fn union_whole(mut u: U) { let c = || unsafe { u.a }; let r = unsafe { &mut u.b }; let _ = c(); *r = 1.0; }
fn struct_disjoint(mut p: P) { let c = || p.a.len(); let r = &mut p.b; let _ = c(); r.push(1); }

fn shared_ref_truncated(r: &P, mut q: P) { let c = || r.a.len(); let m = &mut q.a; let _ = c(); m.push(1); }
fn shared_ref_vs_mut(r: &mut P) { let c = || r.a.len(); let m = &mut r.a; let _ = c(); m.push(1); }

#[repr(packed)]
pub struct Pk { a: u32, b: u32 }
pub struct Unpk { a: u32, b: u32 }
#[repr(C, packed(2))]
pub struct PkS { s: String, t: String }
fn packed_borrow_whole(mut p: Pk) { let c = || p.a; p.b = 1; let _ = c(); }
fn unpacked_borrow_field(mut q: Unpk) { let c = || q.a; q.b = 1; let _ = c(); }
fn packed_move_field(p: PkS) { let c = || take(p.s); take(p.t); c(); }

fn main() {}
"#;

/// What one body's check said: the type check's verdict, the borrow check's codes, and whether
/// either stopped short.
#[derive(Debug)]
struct Said {
    name: &'static str,
    types_clean: bool,
    borrow_codes: Vec<&'static str>,
    stopped: bool,
}

fn verdicts(wanted: &[&'static str]) -> Vec<Said> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/places/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "places", dir: "/places/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2021").expect("Rust 2021"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut out = Vec::new();
    for &name in wanted {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
        let (v, b) = world.check_borrows(BodyRef::Free(key), alloc);
        let said = Said {
            name,
            types_clean: v.is_clean() && v.reads.is_some(),
            borrow_codes: b.findings.iter().map(|d| d.code).collect(),
            stopped: !(v.gaps.is_empty() && v.undecided.is_empty() && v.refused.is_none()) || b.refused.is_some(),
        };
        // The verdicts first, every one, before any assertion.
        println!("{name}: types clean {} borrow codes {:?} stopped {}", said.types_clean, said.borrow_codes, said.stopped);
        if said.stopped {
            println!("  stopped by: gaps {:?} undecided {:?} refused {:?} borrow refused {:?}", v.gaps, v.undecided, v.refused, b.refused);
        }
        out.push(said);
    }
    out
}

/// `clean` is accepted with no borrow finding; each of `refused` reports `code` (and the type
/// check is clean for both: the twins differ only in the places).
fn judge(rows: &[Said], clean: &[&str], refused: &[&str], code: &str) {
    for n in clean {
        let s = rows.iter().find(|s| s.name == *n).expect("checked above");
        assert!(s.types_clean && !s.stopped && s.borrow_codes.is_empty(), "`{n}`: rustc accepts it: {s:?}");
    }
    for n in refused {
        let s = rows.iter().find(|s| s.name == *n).expect("checked above");
        assert!(s.types_clean && !s.stopped, "TWIN `{n}`: a gap where rustc decides {code}: {s:?}");
        assert!(s.borrow_codes.contains(&code), "TWIN `{n}`: rustc reports {code}: {s:?}");
    }
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h1_a_moved_field_leaves_the_rest_live() {
    let rows = verdicts(&["move_field", "move_field_again"]);
    judge(&rows, &["move_field"], &["move_field_again"], "E0382");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h2_a_closure_loan_is_of_its_place() {
    let rows = verdicts(&["disjoint_mut", "overlap_mut"]);
    judge(&rows, &["disjoint_mut"], &["overlap_mut"], "E0502");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h3_tempo_trie_shapes_reads_a_live_field() {
    let rows = verdicts(&["trie_shape", "trie_shape_moved"]);
    judge(&rows, &["trie_shape"], &["trie_shape_moved"], "E0382");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h4_tempo_unwrap_or_else_takes_the_other_fields() {
    let rows = verdicts(&["unwrap_or_else_fields", "unwrap_or_else_same"]);
    judge(&rows, &["unwrap_or_else_fields"], &["unwrap_or_else_same"], "E0382");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h5_tempo_config_moves_a_nested_field() {
    let rows = verdicts(&["nested_field", "nested_field_moved"]);
    judge(&rows, &["nested_field"], &["nested_field_moved"], "E0382");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h6_a_place_through_a_mut_reference_is_disjoint() {
    let rows = verdicts(&["through_mut_ref", "through_mut_ref_overlap"]);
    judge(&rows, &["through_mut_ref"], &["through_mut_ref_overlap"], "E0502");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h7_a_value_with_drop_moves_whole() {
    let rows = verdicts(&["no_drop_field", "drop_type_whole"]);
    judge(&rows, &["no_drop_field"], &["drop_type_whole"], "E0382");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h8_a_union_is_captured_whole() {
    let rows = verdicts(&["struct_disjoint", "union_whole"]);
    judge(&rows, &["struct_disjoint"], &["union_whole"], "E0502");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h9_a_shared_reference_is_captured_as_its_target() {
    let rows = verdicts(&["shared_ref_truncated", "shared_ref_vs_mut"]);
    judge(&rows, &["shared_ref_truncated"], &["shared_ref_vs_mut"], "E0502");
}

#[test]
#[ignore = "stage 2: needs blaze-typeck `ActiveEnv = Places` (the switch removes this)"]
fn h10_a_borrow_of_a_packed_field_captures_the_struct() {
    let rows = verdicts(&["unpacked_borrow_field", "packed_move_field", "packed_borrow_whole"]);
    judge(&rows, &["unpacked_borrow_field", "packed_move_field"], &["packed_borrow_whole"], "E0506");
}
