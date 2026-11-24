//! One fn's row of the program's table, made with a VERDICT: tabled, or left out with the
//! refusal that names why.
//!
//! The rule this file keeps: nothing that goes wrong while making ONE fn's row can end the
//! making of the program. [`table_one`] returns a [`Verdict`], not a `Result`: it has no way
//! to return `Front2Program::make`'s error, so a `?` on a new question of a fn's body (a
//! literal's value, a template, an attribute) lands in the private [`row`], whose every error
//! is that fn's refusal. `make` then decides in ONE place what a refusal of one fn means: a
//! survey files it by [`class`] and goes on (each instance that calls the fn is then refused
//! with that cause: run.rs `callee_refusal`), a run stops with it.
//!
//! Before this file the same decision was written at five places of `make`'s loop and missed
//! at two (`literals(..)?` and `symbols_of(..)?`), so one float literal core's `f16::default`
//! holds ended the walk's whole builds survey.

use super::*;

/// What making one fn's row came to.
pub(super) enum Verdict<'n> {
    /// The row, ready for the table.
    Tabled(Tabled<'n>),
    /// The fn is not tabled: its path (handed back) and the refusal naming why.
    LeftOut { path: Box<str>, refusal: Front2Refusal },
}

/// A row's parts beyond its key and path.
struct Row<'n> {
    body: Option<Body<'n>>,
    checked: Option<Checked<'static>>,
    kind: FnKind,
    at: At,
    abi: Abi,
    nounwind: bool,
    track_caller: bool,
    fn_attrs: blaze_lir::func::FnAttrs,
    symbol: Box<str>,
    export: Option<Box<str>>,
    lits: Literals,
    intrinsic: Option<Intrinsic>,
}

/// The row of fn `key` (shown as `path`): `body` is its lowered and checked body, `None` for
/// a fn with none (a foreign fn, a trait's declaration when `of_trait`, an intrinsic).
pub(super) fn table_one<'n>(
    names: &'n Names<'n>,
    store: &Store<'n>,
    key: ItemKey,
    body: Option<(Body<'n>, Checked<'static>)>,
    path: crate::front2::row_path::RowPath,
    of_trait: bool,
) -> Verdict<'n> {
    match row(names, store, key, body, &path, of_trait) {
        Ok(r) => Verdict::Tabled(Tabled {
            key,
            body: r.body,
            checked: r.checked,
            kind: r.kind,
            path: path.into_text(),
            at: r.at,
            abi: r.abi,
            nounwind: r.nounwind,
            track_caller: r.track_caller,
            fn_attrs: r.fn_attrs,
            symbol: r.symbol,
            export: r.export,
            lits: r.lits,
            intrinsic: r.intrinsic,
        }),
        Err(refusal) => Verdict::LeftOut { path: path.into_text(), refusal },
    }
}

/// [`table_one`]'s work. Every `Err` here is a refusal of THIS fn, named.
fn row<'n>(
    names: &'n Names<'n>,
    store: &Store<'n>,
    key: ItemKey,
    body: Option<(Body<'n>, Checked<'static>)>,
    row_path: &crate::front2::row_path::RowPath,
    of_trait: bool,
) -> Result<Row<'n>, Front2Refusal> {
    // `path` is for messages; the item's identifier, data, is `row_path.ident()` (rule 30).
    let path = row_path.text();
    let function = || Box::<str>::from(path);
    let Some(head) = store.fns.get(&key) else {
        return Err(Front2Refusal::NoHead { function: function() });
    };
    let symbols = symbols_of(names, head.lib, crate::front2::links::values_key(names, store, head.lib), head.text, path)?;
    let (at, foreign) = super::head_abi::placed_fn(names, store, key, head.lib).map_err(|_| Front2Refusal::Unplaced { function: function() })?;
    // A fn an `extern` block in a BODY declares is foreign as a module-level one is: the
    // names library places no local item, so its block's ABI and its declared name are the
    // store's (front2/panic_entry.rs `local_abi`, `local_name`).
    let local = |why: crate::front2::panic_entry::Refused| Front2Refusal::Attrs { function: function(), why: why.form().into() };
    let foreign = foreign.or_else(|| crate::front2::panic_entry::local_abi(store, key));
    if let Some(llvm) = lazy::seam_rules::llvm_row(foreign, symbols.link_name.as_deref(), path)? {
        return Ok(Row {
            body: None,
            checked: None,
            kind: llvm.kind,
            at,
            abi: llvm.build.abi,
            nounwind: llvm.build.nounwind,
            track_caller: llvm.track_caller,
            fn_attrs: symbols.fn_attrs,
            symbol: llvm.symbol.into(),
            export: symbols.export,
            lits: Literals::default(),
            intrinsic: Some(llvm.intrinsic),
        });
    }
    // The head's ABI, a foreign block's overriding it (head_abi.rs: one rule for the table's
    // rows and for the builder's question of a fn left out): the calling convention AND the
    // unwind category ("C" aborts, "C-unwind" unwinds: one `Abi::C` for the builder, the
    // category kept beside it).
    let own = super::head_abi::head_abi(names, store, key, &function)?;
    let declared_name = crate::front2::panic_entry::local_name(store, key).map_err(local)?;
    // Core's `#[lang = "panic_impl"]` declaration is no symbol: a call of it is a call of the
    // program's `#[panic_handler]` fn (the walk queued it; `resolve` answers it).
    let alloc = store.tys.alloc();
    let panic_entry = crate::front2::panic_entry::declares(names, store, key, alloc).map_err(local)?;
    // Alloc's `extern "Rust"` allocator declarations are no symbols either: a call of one is a
    // call of its default (the walk queued it; `resolve` answers it), and the marker
    // `__rust_no_alloc_shim_is_unstable_v2` is built in place as nothing (front2/alloc_shim.rs).
    let alloc_shim = crate::front2::alloc_shim::declares(names, store, key, alloc)
        .map_err(|why| Front2Refusal::Attrs { function: function(), why: why.detail() })?;
    let bound = lazy::seam_rules::bound_decl_row(names, store, key, panic_entry, alloc_shim, path)?;
    let (body, checked, kind, symbol, lits) = match (body, bound) {
        (Some((body, checked)), _) => {
            let lits = literals(&body, path)?;
            (Some(body), Some(checked), FnKind::Free, Box::<str>::from(""), lits)
        }
        // Tabled for its ABI alone (the builder reads a call's ABI from the item the call
        // names): no body, no symbol, never `Foreign`.
        (None, Some((kind, symbol))) => (None, None, kind, symbol.into(), Literals::default()),
        // A declaration in an `extern` block is called by its symbol: its `#[link_name]`,
        // else its name.
        (None, None) if foreign.is_some() && head.container.is_none() => {
            let symbol = symbols.link_name.unwrap_or_else(|| declared_name.unwrap_or(row_path.ident()).into());
            (None, None, FnKind::Foreign, symbol, Literals::default())
        }
        (None, None) => {
            let kind = lazy::seam_rules::rust_row_kind(of_trait, symbols.intrinsic, path)?;
            (None, None, kind, Box::<str>::from(""), Literals::default())
        }
    };
    let intrinsic = match (symbols.intrinsic, &body) {
        // The rule of front2/intrinsic_rule.rs. A fn this walk lowered a body for is built from
        // it (the `_` arm: an intrinsic with a fallback body); one it holds no body for is the
        // table's, and else refused by the reason its declaration gives.
        (true, None) => {
            use crate::front2::intrinsic_rule::{Built, built, declared_at};
            match built(row_path.ident(), symbols.intrinsic_declared) {
                Ok(Built::Table(which)) => Some(Intrinsic::Table(which)),
                // A body the declaration has and this walk did not lower: no body to build.
                Ok(Built::FallbackBody) => return Err(Front2Refusal::NoBody { function: function() }),
                Err(unbuilt) => return Err(Front2Refusal::Intrinsic { function: function(), unbuilt, at: declared_at(names, head.lib) }),
            }
        }
        (false, None) if alloc_shim == Some(crate::front2::alloc_shim::Shim::Marker) => {
            match blaze_build::intrinsics::Which::named(crate::front2::alloc_shim::Shim::Marker.declared()) {
                Ok(which) => Some(Intrinsic::Table(which)),
                Err(_) => {
                    let at = crate::front2::intrinsic_rule::declared_at(names, head.lib);
                    return Err(Front2Refusal::Intrinsic { function: function(), unbuilt: crate::front2::intrinsic_rule::Unbuilt::NoBody, at });
                }
            }
        }
        _ => None,
    };
    // `#[rustc_nounwind]` makes any fn nounwind (core/src/panicking.rs:88-92).
    let nounwind = own.nounwind || symbols.nounwind;
    Ok(Row { body, checked, kind, at, abi: own.abi, nounwind, track_caller: symbols.track_caller, fn_attrs: symbols.fn_attrs, symbol, export: symbols.export, lits, intrinsic })
}

/// The cause a survey files a left-out fn under: the refusal without the fn's own name or
/// place, so every fn left out for one cause is one class (the texts of the four causes the
/// loop filed before this file are kept as they were). A refusal with no shorter name is
/// filed by its whole text.
pub(super) fn class(refusal: &Front2Refusal) -> Box<str> {
    match refusal {
        Front2Refusal::NoHead { .. } => Box::from("no head in the store (front2's)"),
        Front2Refusal::Abi { abi, .. } => format!("ABI {abi:?}: neither Rust nor C").into_boxed_str(),
        Front2Refusal::NoBody { .. } => Box::from("no body (front2's)"),
        Front2Refusal::TargetIntrinsic { .. } => Box::from("a target intrinsic"),
        Front2Refusal::Intrinsic { function, .. } => {
            let name = function.rsplit("::").next().unwrap_or(function);
            format!("the intrinsic `{name}`, which the build's table does not name").into_boxed_str()
        }
        Front2Refusal::Literal { what, .. } => format!("a literal front2 does not read: {what}").into_boxed_str(),
        Front2Refusal::Attrs { why, .. } => format!("its attributes do not read: {why}").into_boxed_str(),
        Front2Refusal::NoRowPath(no) => format!("no row path: {}", no.why).into_boxed_str(),
        other => format!("{other}").into_boxed_str(),
    }
}

/// What `make`'s loop does with a fn that is not tabled, the ONE place a fn's refusal turns
/// into the survey's row or the run's error: a survey (`left_out` is `Some`) files `(path,
/// class)` and the key's cause in `left` and goes on (`Ok`); a run stops with the refusal.
pub(super) fn left_out_or_stop(
    key: ItemKey,
    path: Box<str>,
    refusal: Front2Refusal,
    left: &mut HashMap<ItemKey, Box<str>, Global>,
    left_out: Option<&mut Vec<(Box<str>, LeftOut), Global>>,
) -> Result<(), Front2Refusal> {
    let Some(out) = left_out else { return Err(refusal) };
    let class = class(&refusal);
    left.insert(key, class.clone());
    out.push((path, LeftOut::named(class, refusal)));
    Ok(())
}

/// A bodied fn's row before [`table_one`]: `body` lowered again into the program's table
/// `tys`, with its row path. `Err` is that ONE fn's refusal for [`left_out_or_stop`], with the
/// name it is filed under: no row path (`NoRowPath`, filed by the item's shown name; the body
/// is then NOT lowered again: rule 77), or a body that does not lower again (`Lower`, filed by
/// its row path). Before this, `make` returned either from the whole program, so in a survey
/// one such fn ended the census (survey-left-out-1).
#[allow(clippy::too_many_arguments)]
pub(super) fn lowered_row<'n>(
    names: &'n Names<'n>,
    prepared: &Prepares<'n>,
    keys: &mut crate::front2::keys::Keys,
    store: &mut Store<'n>,
    body: &Body<'n>,
    tys: &mut PortableTys<'static>,
    path: Result<crate::front2::row_path::RowPath, crate::front2::row_path::NoRowPath>,
) -> Result<(Body<'n>, crate::front2::row_path::RowPath), (Box<str>, Front2Refusal)> {
    let path = path.map_err(|no| (no.item.boxed(), Front2Refusal::NoRowPath(no)))?;
    match lower_again(names, prepared, keys, store, body, tys) {
        Ok(again) => Ok((again, path)),
        Err(e) => Err((path.into_text(), Front2Refusal::Lower(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal() -> Front2Refusal {
        Front2Refusal::Literal { function: Box::from("core::default::default"), what: "this float literal", range: tc::Range { start: 94, end: 100 } }
    }

    /// The walk's fault of 2026-11-16 (list-AG.txt: "BUILDS: no survey: front2:
    /// `..::default` at 94..100: this float literal (front2's)"): a survey must file one fn's
    /// refusal under its cause and go on; only a run stops with it.
    ///
    /// Prints: `test front2::program::tabled::tests::a_survey_files_one_fns_refusal_and_goes_on ... ok`
    #[test]
    fn a_survey_files_one_fns_refusal_and_goes_on() {
        let key = ItemKey(7);
        let mut left = HashMap::new_in(Global);
        let mut out = Vec::new_in(Global);
        let went_on = left_out_or_stop(key, Box::from("core::default::default"), literal(), &mut left, Some(&mut out));
        assert!(went_on.is_ok(), "a survey goes on after one fn's refusal");
        assert_eq!(out.len(), 1);
        assert_eq!(&*out[0].0, "core::default::default");
        assert_eq!(&*out[0].1.class, "a literal front2 does not read: this float literal");
        assert!(matches!(out[0].1.refusal, Front2Refusal::Literal { what: "this float literal", .. }), "the row keeps the typed refusal");
        assert_eq!(left.get(&key).map(|c| &**c), Some("a literal front2 does not read: this float literal"));
        // A run has no list to file in: the same refusal stops it, named.
        let stopped = left_out_or_stop(key, Box::from("core::default::default"), literal(), &mut left, None);
        assert!(matches!(stopped, Err(Front2Refusal::Literal { what: "this float literal", .. })));
    }

    /// Census run 3 (2026-11-16 21:00, tempo's build census walked from `main`) stopped the
    /// WHOLE survey: "no row path is built for an item the names source holds no path for (key
    /// 2980e808e42ac8c9..)". The rule (this file's, and `build_survey`'s doc): a survey files one
    /// item's refusal under its cause and goes on; a run stops with it, named (rule 3).
    ///
    /// The item is the census's: checker key 0x2980e808e42ac8c9057e8231da5614fc (core's crate
    /// prefix, an item no names source here holds), over hello world's names, so
    /// `RowPath::of` refuses it the way it did in the census ("the key map and the names source
    /// hold no item under its key"). That `NoRowPath`, given to `left_out_or_stop` as `make`
    /// gives it (refusals-name-items-3's table loop; survey-left-out-1's `lowered_row` gives the
    /// same pair): in a survey it is ONE row, by the item's shown name (its whole key), under
    /// the class "no row path: <why>", and its key's cause is in `left` for a caller's build;
    /// in a run it is `Front2Refusal::NoRowPath` naming the key.
    /// CONTROL: the refusal before refusals-name-items-3, `row_path_of(..).map_err(NoRowPath)?`,
    /// is mode-blind: the same `Err` in a survey, the census's stop (asserted last).
    ///
    /// Prints: `NO ROW PATH: survey: an item the names source holds no path for (key
    /// 2980e808e42ac8c9057e8231da5614fc) -> no row path: the key map and the names source hold
    /// no item under its key` and `NO ROW PATH: run: front2: no row path is built for ..`.
    #[test]
    fn a_survey_files_an_item_with_no_row_path_and_goes_on() {
        const HELLO: &str = "fn main() {}\n";
        let home = std::env::var("HOME").expect("HOME is set");
        let shelf = format!("{home}/.blaze/v2/toolchain");
        let tc = crate::crates::Toolchain::open(&mut crate::crates::DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
        let files = [(String::from("/hello-world/src/main.rs"), String::from(HELLO))];
        let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
        let user = crate::crates::UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = crate::crates::Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
        let crates = inputs.crates();
        // The store's tables are `'static` (heads.rs `Store::new`): until the test grant hands
        // one down, the global allocator is the only `Alloc<'static>` a test can name.
        let alloc = blaze_own::Alloc::global();
        let names = inputs.resolve(&crates, erased.bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let sources = [SourceFile { path: "src/main.rs", text: HELLO }];
        let front = crate::front2::Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
        let store = Store::new(front.index.lang("sized"), front.frame, alloc);
        let ghost = ItemKey(0x2980e808e42ac8c9057e8231da5614fc);
        let made = crate::front2::row_path::RowPath::of(&names, &front.keys, &store, ghost);
        let no = made.clone().expect_err("no names source here holds the census's key");
        assert_eq!(no.why, "the key map and the names source hold no item under its key");
        // A survey: one row, by the item's whole key, under the class; `left` holds the cause.
        let mut left = HashMap::new_in(Global);
        let mut out = Vec::new_in(Global);
        let went_on = left_out_or_stop(ghost, no.item.boxed(), Front2Refusal::NoRowPath(no.clone()), &mut left, Some(&mut out));
        assert!(went_on.is_ok(), "a survey goes on past an item with no row path");
        assert_eq!(out.len(), 1);
        println!("NO ROW PATH: survey: {} -> {}", out[0].0, out[0].1);
        assert!(out[0].0.contains("2980e808e42ac8c9057e8231da5614fc"), "filed by its whole key: {}", out[0].0);
        assert_eq!(&*out[0].1.class, "no row path: the key map and the names source hold no item under its key");
        assert_eq!(left.get(&ghost).map(|c| &**c), Some("no row path: the key map and the names source hold no item under its key"));
        // A run: the same item refuses the program, named (no fallback, rule 3).
        let stopped = left_out_or_stop(ghost, no.item.boxed(), Front2Refusal::NoRowPath(no.clone()), &mut left, None);
        let Err(refusal) = stopped else { panic!("a run stops on an item with no row path") };
        let said = format!("{refusal}");
        println!("NO ROW PATH: run: {said}");
        assert!(matches!(refusal, Front2Refusal::NoRowPath(ref r) if *r == no));
        assert!(said.contains("2980e808e42ac8c9057e8231da5614fc"), "the run's refusal names the key: {said}");
        // CONTROL: the form before -3 refused in every mode, a survey's too.
        let before: Result<crate::front2::row_path::RowPath, Front2Refusal> = made.map_err(Front2Refusal::NoRowPath);
        assert!(before.is_err(), "control: `row_path_of(..).map_err(NoRowPath)?` ends a survey");
    }

    /// The causes the loop filed before keep their texts (the walk's lists group by them).
    ///
    /// Prints: `test front2::program::tabled::tests::the_filed_causes_keep_their_texts ... ok`
    #[test]
    fn the_filed_causes_keep_their_texts() {
        let f = || Box::<str>::from("a::b::ctpop_like");
        assert_eq!(&*class(&Front2Refusal::NoHead { function: f() }), "no head in the store (front2's)");
        assert_eq!(&*class(&Front2Refusal::NoBody { function: f() }), "no body (front2's)");
        let unbuilt = crate::front2::intrinsic_rule::Unbuilt::NoBody;
        assert_eq!(&*class(&Front2Refusal::Intrinsic { function: f(), unbuilt, at: Box::from("a.rs:1") }), "the intrinsic `ctpop_like`, which the build's table does not name");
    }
}
