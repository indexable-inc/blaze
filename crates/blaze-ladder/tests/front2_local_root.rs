//! A fn declared in a body, written by a macro's expansion, checked as a ROOT (hello-main-ctorfn,
//! 2026-11-16). The owner's hello world stopped with "front2: `main` did not check: Gap { owner:
//! ItemKey(0x01b906915739b457a071..), gaps: [CtorFn(<the same key>), Fill(NoDetail(<its library
//! key>))] }": bstd's `main!` (blaze-std entry.rs) writes, inside the launcher `fn main() ->
//! Exit`, the user's `fn main(..)` and `#[unsafe(no_mangle)] pub extern "C" fn
//! bstd_hosted_start(..)`, and the program checks such a body-local fn as a root (program.rs:
//! the enclosing bodies are lowered first, then the entry). Its key has `LibKey::INNER` set (byte
//! 8 is 0xa0): a key front2 derives (nested_exports.rs `local_key`), which the names library
//! keys no outline entry for. Asked for its OWN signature by its own check, the world read "no
//! outline entry" as "a variant" (world_items.rs `fn_sig`: `None => match self.enum_of(lib)`,
//! then `None => Gap::CtorFn`), and its predicates' want read it as "no detail"
//! (fill.rs `want_of_item`: `names.detail_of(lib).ok_or(FillError::NoDetail(lib))`).
//!
//! The rules. The Reference, "Statements", "Item declarations" [statement.item.intro]: "An item
//! declaration statement has a syntactic form identical to an item declaration within a module";
//! [statement.item.scope]: "Declaring an item within a statement block restricts its scope to the
//! block containing the statement." "Functions" [items.fn.intro]: a function item has a
//! signature and a body whatever block it is declared in, so a body-local fn's own body is
//! checked against its own signature, as a module's fn's is; a path to it is a path to a fn,
//! never to a constructor ("Paths", "Path expressions": a tuple struct's or variant's constructor
//! is a value of the type namespace's item, and `caller`/`answer` below are fns). "Macros By
//! Example", "Transcribing": the expansion's items are items like written ones.
//!
//! The program: `write_pair!()` writes `fn holder()` whose body declares `fn answer() -> u8` and
//! `fn caller() -> u8 { answer() }` (a sibling call). `caller` is checked as a root (its head
//! NOT made first, as the program's entry is not), and must check clean.
//!
//! Expected line (stdout): `LOCAL ROOT: caller is a fn by its key (Fn), checked clean; answer
//! is a fn by its key (Fn)`.
//!
//! Control (the cause in: keys.rs `Keys::local` not called from locals.rs, or by_origin.rs
//! `kind_of_key` not reading `Keys::local_kind`, or world_items.rs `fn_sig`'s old arm): the
//! panic names `caller`'s kind as `Err(NoDetail(..))` and its check as `Gap { owner: <caller>,
//! gaps: [CtorFn(<caller>), Fill(NoDetail(..))] }`, the hello's stop in small. Needs
//! `$HOME/.blaze/v2/toolchain`. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::by_origin::{ItemKind, KeyKind, kind_of_key};
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::locals::LocalKind;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "macro_rules! write_pair {
    () => {
        fn holder() -> u8 {
            fn answer() -> u8 { 7 }
            fn caller() -> u8 { answer() }
            caller()
        }
    };
}
write_pair!();

fn main() {}
";

#[test]
fn a_body_local_fn_checks_as_a_root() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/localroot/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "localroot", dir: "/localroot/", root: "/localroot/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;

    // The enclosing body, lowered first as program.rs lowers an entry's enclosing bodies: its
    // local items are kept (locals.rs `Locals::keep`), each key filed with its kind.
    let holder = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["holder"] }, Alloc::global())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`holder` (written by `write_pair!`) from the crate root"));
    store.ensure(&names, &mut front.keys, Want::Fn(holder)).unwrap_or_else(|e| panic!("holder's head: {e:?}"));
    lower_fn(&names, prepared, &mut front.keys, &mut store, holder).unwrap_or_else(|e| panic!("holder's body: {e}"));
    let rows = store.locals.body(checker_of(holder)).map(|b| b.rows).unwrap_or_else(|| panic!("holder's body keeps its local items"));
    let local = |name: &str| rows.iter().find(|r| r.name == name && r.kind == LocalKind::Fn).map(|r| r.key).unwrap_or_else(|| panic!("`{name}` among holder's local fns"));
    let (caller, answer) = (local("caller"), local("answer"));

    // What each key is, by the one classifier: a fn, never "no detail" or a variant.
    // (Compared here: a `KeyKind` borrows the key map, which the check below takes mutably.)
    let caller_kind = kind_of_key(&names, &front.keys, checker_of(caller));
    let caller_is_fn = caller_kind == Ok(KeyKind::Item(ItemKind::Fn(caller)));
    let caller_kind = format!("{caller_kind:?}");
    let answer_kind = kind_of_key(&names, &front.keys, checker_of(answer));
    let answer_is_fn = answer_kind == Ok(KeyKind::Item(ItemKind::Fn(answer)));
    let answer_kind = format!("{answer_kind:?}");

    // `caller` checked as a root: its own head is NOT made first (the program's entry is not).
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, caller).unwrap_or_else(|e| panic!("caller's body: {e}"));
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("spelling");
    let scope = body.scope(&spell);
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let checked = check_body(&mut records, &body, &scope).map(|_| ()).map_err(|e| format!("{e:?}"));

    assert!(caller_is_fn, "a body-local fn's key is a fn by the kind its body's lowering filed, not {caller_kind} (checked: {checked:?})");
    assert!(answer_is_fn, "the sibling body-local fn's key is a fn, not {answer_kind}");
    assert_eq!(checked, Ok(()), "a body-local fn written by an expansion checks as a root, its sibling call typed");
    println!("LOCAL ROOT: caller is a fn by its key (Fn), checked clean; answer is a fn by its key (Fn)");
}
