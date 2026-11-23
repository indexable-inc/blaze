//! A member of a trait or impl is lowered from its own head, read with the marks of the text
//! that wrote it, whatever wrote it (census r16 of tempo, classes A, B and C; front2 body.rs
//! `lower_member_into`, item_site.rs `head_marks`, heads/member_fn.rs `member_head_want`).
//!
//! The Reference, "Macros By Example": a macro call may stand where an associated item may
//! ("Macros can be invoked ... as ... associated items"), so an impl a macro writes may hold a
//! member another call inside it writes: core's `ord_impl!` writes `const impl Ord for $t {
//! .. min_max_impl!($t); }`, num-traits' `impl ToPrimitive for $T` holds
//! `impl_to_primitive_uint_to_uint!` calls. Such a member's text is a slice of the INNER call's
//! expansion, not of its container's.
//!
//! Rows, over the installed toolchain under `$HOME/.blaze/v2/toolchain` (real core), one
//! program:
//! - (a) `nested`: `outer_impl!(W)` writes `impl Tr for W { inner_m!(); }`, `inner_m!` writes
//!   `fn m(&self) -> u16 { self.0 }`. `m`'s body lowers and checks clean; its head's text is a
//!   slice of the expansion of the call the names library says wrote it (a member call: it has
//!   no place among the items); its marks are found by `head_marks`. THE CONTROL (the cause, class A): the container's marks
//!   (`member_marks(.., None, ..)`, what the body read when it re-found its member by comparing
//!   front2's member key with the library's, which never matched) refuse it `NotInText`.
//! - (b) `anon`: `inherent!(W)` writes `impl W { picker!(); }`, `picker!` writes `fn pick(bytes:
//!   [u8; core::mem::size_of::<Self>()]) -> u8`. `own_two` (a `[u8; 2]`) checks clean, and
//!   `own_three` (a `[u8; 3]`) is a diagnostic of the program: the anonymous constant
//!   `size_of::<Self>()` lowered (class B: its marks were asked of its module's place, which
//!   has none, `NoPlace`), was evaluated to 2, and a wrong length is not a hole.
//! - (c) `derived`: the `eq` of `#[derive(PartialEq)] enum E`, asked FIRST (after its impl's
//!   head, which makes no fn's signature: the control is that `store.fns` lacks it then): its
//!   body lowers and its head is in the store after (class C: a member whose head was not made
//!   was `NoHead`, and one whose head was refused lost the reason).
//!
//! NOT RUN by the writer (a write-only lane): uncompiled.
//!
//! What it prints when it passes:
//! ```text
//! front2_member_records: nested: clean, written by the inner call, marks found
//! front2_member_records: anon: own_two: clean; own_three: diags [..]
//! front2_member_records: derived: lowered, head made
//! test members_lower_by_their_own_heads ... ok
//! ```

#![feature(allocator_api)]

use blaze_id::ItemKey;
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{Body, BodyError, lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::item_site::{MarksLost, head_marks, member_marks};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::expansion_origins::offset_in;
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::Names;
use blaze_surface::{ItemDetail, ItemKind, Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

const TEXT: &str = "pub struct W(u16);

pub trait Tr {
    fn m(&self) -> u16;
}

macro_rules! inner_m { () => { fn m(&self) -> u16 { self.0 } } }
macro_rules! outer_impl { ($t:ident) => { impl Tr for $t { inner_m!(); } } }
outer_impl!(W);

macro_rules! picker { () => { fn pick(bytes: [u8; core::mem::size_of::<Self>()]) -> u8 { bytes[1] } } }
macro_rules! inherent { ($t:ident) => { impl $t { picker!(); } } }
inherent!(W);

fn own_two(b: [u8; 2]) -> u8 { W::pick(b) }
fn own_three(b: [u8; 3]) -> u8 { W::pick(b) }

#[derive(PartialEq)]
pub enum E {
    A,
    B(u8),
}

fn main() {}
";

/// The one impl of the program that holds a member named `member`.
fn impl_with(names: &Names<'_>, member: &str) -> ItemKey {
    let impls: Vec<ItemKey> = names
        .items(0)
        .map(|(key, _, _)| key)
        .filter(|&key| NamesSource::detail_of(names, key).is_some_and(|d| matches!(d, ItemDetail::Impl)))
        .filter(|&key| {
            let mut has = false;
            NamesSource::each_member(names, key, &mut |m| has |= m.name == Some(member));
            has
        })
        .collect();
    let [one] = impls.as_slice() else { panic!("the program holds {} impls with a member `{member}`, not one: {impls:?}", impls.len()) };
    *one
}

/// The call that wrote the member `name` of `owner`, as the names library lists it. Its text is
/// read from the head the store holds, never copied: marks are found by the ADDRESS of a slice
/// of the expansion text (blaze-scope `offset_in`), so a copy is in no expansion.
fn written_by(names: &Names<'_>, owner: ItemKey, name: &str) -> Option<ItemKey> {
    let mut found = None;
    NamesSource::each_member(names, owner, &mut |m| {
        if m.name == Some(name) {
            found = Some(m.written_by);
        }
    });
    found.unwrap_or_else(|| panic!("{owner:x} lists no member `{name}`"))
}

/// `body` checked: `clean`, or what the check said.
fn checked<'n, S: NamesSource<'n>>(records: &mut Records<'_, 'n, S>, body: &Body<'_>) -> String {
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("the body's spelling");
    let scope = body.scope(&spell);
    match check_body(records, body, &scope) {
        Ok(done) if done.diags.is_empty() => "clean".to_string(),
        Ok(done) => format!("diags {:?}", done.diags),
        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
        Err(e) => format!("not checked: {e}"),
    }
}

/// Rows (a), (b) and (c) over one program and one store.
///
/// Prints the three `front2_member_records:` lines of the module's doc and
/// `test members_lower_by_their_own_heads ... ok`.
#[test]
fn members_lower_by_their_own_heads() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/members/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "members", dir: "/members/", root: "/members/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut failed: Vec<String> = Vec::new();

    // (a) A member a call nested in its container's body wrote.
    let tr_impl = impl_with(&names, "m");
    let m_by = written_by(&names, tr_impl, "m");
    match store.ensure(&names, &mut front.keys, Want::Impl(tr_impl)) {
        Err(e) => failed.push(format!("nested: the impl's head: {e:?}")),
        Ok(()) => match front.keys.filed_member(checker_of(tr_impl), MemberKind::Fn, "m") {
            None => failed.push("nested: the impl's head files no fn `m`".to_string()),
            Some(m) => {
                let mut said = Vec::new();
                match lower_member(&names, prepared, &mut front.keys, &mut store, m) {
                    Err(e) => failed.push(format!("nested: `m` does not lower: {e}")),
                    Ok(body) => {
                        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                        let got = checked(&mut records, &body);
                        if got != "clean" {
                            failed.push(format!("nested: `m`: {got}"));
                        }
                        said.push(got);
                    }
                }
                // Its text, as its head holds it (the expansion's rendering: `fn m( & self) ->
                // u16 { self. 0 }`, tokens spaced as rendered, so compared without spaces).
                let Some(m_text) = store.fns.get(&m).map(|head| head.text) else {
                    panic!("nested: `m` lowered with no head in the store");
                };
                let squeezed: String = m_text.chars().filter(|c| !c.is_whitespace()).collect();
                if !squeezed.starts_with("fnm(&self)->u16{self.0}") {
                    failed.push(format!("nested: `m`'s head holds the text {m_text:?}, not `inner_m!`'s fn"));
                }
                // Its place: written by a call in the impl's body (a MEMBER call, keyed under its
                // owner like any member, so it has no place among the items: blaze-scope
                // task/lower.rs `Lower::assoc`), whose own expansion holds `m`'s text.
                match m_by.map(|call| (call, NamesSource::text_marks(&names, 0, call))) {
                    Some((_, Ok(Some(marks)))) if offset_in(marks.text, m_text).is_some() => said.push("written by the inner call".to_string()),
                    Some((call, Ok(Some(_)))) => failed.push(format!("nested: `m`'s text is not in the expansion of {call:x}, the call the names library says wrote it")),
                    Some((call, Ok(None))) => said.push(format!("written by {call:x}, whose expansion keeps no column")),
                    Some((call, Err(e))) => failed.push(format!("nested: the expansion of {call:x}: {e}")),
                    None => failed.push("nested: the names library says `m` is written in the impl's own body, not by `inner_m!`".to_string()),
                }
                match head_marks(&names, tr_impl, "m", ItemKind::Function, m_text) {
                    Ok(Some(marks)) if offset_in(marks.text, m_text).is_some() => said.push("marks found".to_string()),
                    // A text the inner call's expansion keeps no column for (blaze-scope
                    // `OriginsError::Rewritten`) is read at its module: no marks, by design.
                    Ok(None) => said.push("no column, read at its module".to_string()),
                    Ok(Some(_)) => failed.push("nested: `m`'s marks are another text's".to_string()),
                    Err(e) => failed.push(format!("nested: `m`'s marks: {e}")),
                }
                // THE CONTROL: the container's marks (the cause) do not hold `m`'s text.
                match member_marks(&names, tr_impl, None, m_text) {
                    Err(MarksLost::NotInText { .. }) => {}
                    other => failed.push(format!("nested: THE CONTROL: the container's marks of `m` were {:?}, not `NotInText` (is `m` really the inner call's?)", other.map(|o| o.is_some()))),
                }
                println!("front2_member_records: nested: {}", said.join(", "));
            }
        },
    }

    // (b) An anonymous constant over a ground `Self` in a macro-written fn's signature.
    let mut anon = Vec::new();
    for (n, must_be_clean) in [("own_two", true), ("own_three", false)] {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: the constant {c:x} its body names: {e}");
                    }
                }
                Err(e) => break format!("not lowered: {e}"),
                Ok(body) => {
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    break checked(&mut records, &body);
                }
            }
        };
        match (must_be_clean, outcome.as_str()) {
            (true, "clean") => {}
            (false, o) if o.starts_with("diags ") => {}
            (true, o) => failed.push(format!("anon: `{n}` must check clean: {o}")),
            (false, o) => failed.push(format!("anon: `{n}` must be a diagnostic of the program (a `[u8; 3]` where `[u8; 2]` is expected), not clean and not a gap: {o}")),
        }
        anon.push(format!("{n}: {}", outcome.chars().take(120).collect::<String>()));
    }
    println!("front2_member_records: anon: {}", anon.join("; "));

    // (c) A derived impl's fn asked first: its head is made by the body that needs it.
    let derived = impl_with(&names, "eq");
    match store.ensure(&names, &mut front.keys, Want::Impl(derived)) {
        Err(e) => failed.push(format!("derived: the impl's head: {e:?}")),
        Ok(()) => match front.keys.filed_member(checker_of(derived), MemberKind::Fn, "eq") {
            None => failed.push("derived: the impl's head files no fn `eq`".to_string()),
            Some(eq) => {
                // The control: the impl's head makes no fn's signature (heads/member_fn.rs).
                let made_before = store.fns.contains_key(&eq);
                match lower_member(&names, prepared, &mut front.keys, &mut store, eq) {
                    Ok(_) if !made_before && store.fns.contains_key(&eq) => println!("front2_member_records: derived: lowered, head made"),
                    Ok(_) if made_before => failed.push("derived: THE CONTROL: `eq`'s head was made by its impl's head, so it was not asked first".to_string()),
                    Ok(_) => failed.push("derived: `eq` lowered and its head is not in the store".to_string()),
                    Err(BodyError::NoHead(_)) => failed.push("derived: `eq` is `NoHead`: no want made its head".to_string()),
                    Err(e) => failed.push(format!("derived: `eq` does not lower: {e}")),
                }
            }
        },
    }

    assert!(failed.is_empty(), "members that did not lower by their own heads: {failed:#?}");
}
