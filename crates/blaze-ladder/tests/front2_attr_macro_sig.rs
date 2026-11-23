//! An attribute macro that rewrites its fn's signature (evm2's `#[instruction]` on tempo_evm's
//! `pub fn configurable_account(cx: _) -> Result`, crates/evm/src/instructions.rs:141): the fn
//! front2 heads and checks is the macro's OUTPUT, never the item as written.
//!
//! The rule. The Reference, "Procedural Macros", "Attribute macros"
//! (macro.proc.attribute.behavior): "The item to which the attribute is applied is replaced by
//! the zero or more items in the returned TokenStream." So `cx: _`, which no fn signature may
//! hold (the Reference, "Inferred type": "the inferred type cannot be used in item
//! signatures"), is never checked: the macro writes the parameter's type.
//!
//! The cause in census 14 (tree 4685c8b5b7, 16:09): blaze-scope task/lower.rs (then line 1876)
//! pushed the item's own text into the texts table BEFORE the attribute-macro branch returned,
//! so the written `fn configurable_account(cx: _)` stood as an item with a file place beside
//! the expansion: its head refused `out`/`Word` for `millis_timestamp`/`sstore` (census
//! "HEAD REFUSALS ... `out`", "`Word`") and its body gave E0282 at `cx` for
//! `configurable_account`. Fixed at the one home in d947642cb0 (17:22: the text is pushed only
//! after that branch, task/lower.rs `// The item's own text, only now`), proved at the names
//! layer by blaze-scope tests/items_attr_text.rs. This test proves the same through front2's
//! heads, lowering and checker, with the shape of the census row: the input's `cx: _` and a
//! name the output alone declares.
//!
//! The macro is an attribute `macro_rules!` (an `attr()` arm, as blaze-scope's
//! tests/items.rs and tests/builtin_attrs_reach_cells.rs write one): what matters is that its
//! output replaces the item, which is the same for a procedural attribute macro.
//!
//! Expected lines (in any order): `spend: clean`, `calls_spend: clean`, and
//! `spend: one text, the expansion's`. Control: with task/lower.rs's `self.out.texts.push(..)`
//! moved back above the `if let Some(path) = written..` branch, `spend` resolves to the item as
//! written and fails ("spend: the text checked is the input's ..." and a diagnostic at `cx`).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main() {}
pub struct Ctx { pub gas: u64 }
macro_rules! instruction {
    attr() ($v:vis fn $name:ident(cx: _) -> $r:ty $body:block) => {
        $v fn $name(cx: &Ctx) -> $r $body
    };
}
#[instruction]
pub fn spend(cx: _) -> u64 { cx.gas }
fn calls_spend() -> u64 { spend(&Ctx { gas: 3 }) }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("spend", "the expansion's `fn spend(cx: &Ctx) -> u64`: `cx.gas` is a field of `Ctx`"),
    ("calls_spend", "a caller sees the expansion's head: `spend(&Ctx { .. })` is `u64`"),
];

#[test]
fn an_attribute_macros_output_is_the_fn_checked_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/attrsig/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "attrsig", dir: "/attrsig/", root: "/attrsig/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let heap = blaze_own::testing::heap();
    let find = |segments: &[&str]| {
        names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from the crate root"))
    };
    let mut report: Vec<String> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();

    // A text with every whitespace character removed.
    let squeezed = |s: &str| s.split_whitespace().collect::<String>();
    // The one `spend` with a text is the expansion's: the input `cx: _` has none.
    let spend_texts: Vec<&str> = names.items(0).filter_map(|(k, _, _)| names.item_text(k)).filter(|t| t.name == Some("spend")).map(|t| t.text).collect();
    match spend_texts.as_slice() {
        // An expansion's text is rendered from its tokens (`cx: & Ctx`): compared with no whitespace.
        [one] if !squeezed(one).contains("cx:_") && squeezed(one).contains("cx:&Ctx") => report.push("spend: one text, the expansion's".to_string()),
        other => {
            report.push(format!("spend: the texts named `spend` are {other:?}; want one, the expansion's `cx: &Ctx`"));
            failed.push("spend texts");
        }
    }
    let spend = find(&["spend"]);
    match names.item_text(spend) {
        Some(t) if squeezed(t.text).contains("cx:_") => {
            report.push(format!("spend: the text checked is the input's {:?}, not the expansion's", t.text));
            failed.push("spend text");
        }
        Some(_) => {}
        None => {
            report.push("spend: the resolved `spend` has no text".to_string());
            failed.push("spend text");
        }
    }

    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    for &(name, proves) in CLEAN {
        let key = find(&[name]);
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            report.push(format!("{name}: no head: {e:?} ({proves})"));
            failed.push(name);
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => b,
            Err(e) => {
                report.push(format!("{name}: does not lower: {e} ({proves})"));
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, &body, &scope) {
            Ok(_) => report.push(format!("{name}: clean")),
            Err(CheckFail::Diags { diags, .. }) => {
                report.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            Err(other) => {
                report.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name);
            }
        }
    }
    // Failures first: the harness's runner shows the first lines only.
    report.sort_by_key(|l| l.contains(": clean") || l.contains(": one text"));
    for line in &report {
        println!("{line}");
    }
    let why: Vec<String> = report.iter().filter(|l| !l.contains(": clean") && !l.contains(": one text")).map(|l| l.chars().take(300).collect()).collect();
    assert!(failed.is_empty(), "what did not hold: {failed:?}\n{}", why.join("\n"));
}
