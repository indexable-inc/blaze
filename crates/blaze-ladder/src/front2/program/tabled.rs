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
    path: Box<str>,
    of_trait: bool,
) -> Verdict<'n> {
    match row(names, store, key, body, &path, of_trait) {
        Ok(r) => Verdict::Tabled(Tabled {
            key,
            body: r.body,
            checked: r.checked,
            kind: r.kind,
            path,
            at: r.at,
            abi: r.abi,
            symbol: r.symbol,
            export: r.export,
            lits: r.lits,
            intrinsic: r.intrinsic,
        }),
        Err(refusal) => Verdict::LeftOut { path, refusal },
    }
}

/// [`table_one`]'s work. Every `Err` here is a refusal of THIS fn, named.
fn row<'n>(
    names: &'n Names<'n>,
    store: &Store<'n>,
    key: ItemKey,
    body: Option<(Body<'n>, Checked<'static>)>,
    path: &str,
    of_trait: bool,
) -> Result<Row<'n>, Front2Refusal> {
    let function = || Box::<str>::from(path);
    let Some(head) = store.fns.get(&key) else {
        return Err(Front2Refusal::NoHead { function: function() });
    };
    let abi = match head.header.abi {
        tc::Abi::Rust => Abi::Rust,
        tc::Abi::C => Abi::C,
        tc::Abi::RustCall => Abi::RustCall,
        // The builder spreads no other ABI's arguments.
        abi => return Err(Front2Refusal::Abi { function: function(), abi }),
    };
    let symbols = symbols_of(names, head.lib, head.text, path)?;
    let (at, foreign) = placed(names, head.lib);
    // A fn an `extern` block in a BODY declares is foreign as a module-level one is: the
    // names library places no local item, so its block's ABI and its declared name are the
    // store's (front2/panic_entry.rs `local_abi`, `local_name`).
    let local = |why: crate::front2::panic_entry::Refused| Front2Refusal::Attrs { function: function(), why: why.form().into() };
    let foreign = foreign.or_else(|| crate::front2::panic_entry::local_abi(store, key));
    let declared_name = crate::front2::panic_entry::local_name(store, key).map_err(local)?;
    // Core's `#[lang = "panic_impl"]` declaration is no symbol: a call of it is a call of the
    // program's `#[panic_handler]` fn (the walk queued it; `resolve` answers it).
    let panic_entry = crate::front2::panic_entry::declares(names, store, key, Alloc::global()).map_err(local)?;
    // Alloc's `extern "Rust"` allocator declarations are no symbols either: a call of one is a
    // call of its default (the walk queued it; `resolve` answers it), and the marker
    // `__rust_no_alloc_shim_is_unstable_v2` is built in place as nothing (front2/alloc_shim.rs).
    let alloc_shim = crate::front2::alloc_shim::declares(names, store, key, Alloc::global())
        .map_err(|why| Front2Refusal::Attrs { function: function(), why: why.detail() })?;
    let (body, checked, kind, symbol, lits) = match body {
        Some((body, checked)) => {
            let lits = literals(&body, path)?;
            (Some(body), Some(checked), FnKind::Free, Box::<str>::from(""), lits)
        }
        // Tabled for its ABI alone (the builder reads a call's ABI from the item the call
        // names): no body, no symbol, never `Foreign`.
        None if panic_entry => (None, None, FnKind::Free, Box::<str>::from(""), Literals::default()),
        None if alloc_shim.is_some() => (None, None, FnKind::Free, Box::<str>::from(""), Literals::default()),
        // A declaration in an `extern` block is called by its symbol: its `#[link_name]`,
        // else its name.
        None if foreign.is_some() && head.container.is_none() => {
            let symbol = symbols.link_name.unwrap_or_else(|| declared_name.unwrap_or_else(|| path.rsplit("::").next().unwrap_or(path)).into());
            (None, None, FnKind::Foreign, symbol, Literals::default())
        }
        // A trait's fn called on a parameter or `dyn`: never built (each instance resolves to
        // an impl's fn); its container index is the old front's and front2 numbers no traits,
        // so it reads 0, which nothing asks.
        None if of_trait => (None, None, FnKind::Trait(0), Box::<str>::from(""), Literals::default()),
        // An intrinsic: the build makes each call from its table, by the fn's name.
        None if symbols.intrinsic => (None, None, FnKind::Free, Box::<str>::from(""), Literals::default()),
        None => return Err(Front2Refusal::NoBody { function: function() }),
    };
    let intrinsic = match (symbols.intrinsic, &body) {
        (true, None) => {
            let name = path.rsplit("::").next().unwrap_or(path);
            match blaze_build::intrinsics::Which::named(name) {
                Ok(which) => Some(Intrinsic::Table(which)),
                Err(_) => return Err(Front2Refusal::Intrinsic { function: function() }),
            }
        }
        (false, None) if alloc_shim == Some(crate::front2::alloc_shim::Shim::Marker) => {
            match blaze_build::intrinsics::Which::named(crate::front2::alloc_shim::Shim::Marker.declared()) {
                Ok(which) => Some(Intrinsic::Table(which)),
                Err(_) => return Err(Front2Refusal::Intrinsic { function: function() }),
            }
        }
        _ => None,
    };
    // A foreign fn's own text carries no ABI: its block's does.
    let abi = match foreign {
        None => abi,
        Some("C" | "C-unwind") => Abi::C,
        Some("Rust") => Abi::Rust,
        Some("rust-call") => Abi::RustCall,
        Some(_) => return Err(Front2Refusal::Abi { function: function(), abi: head.header.abi }),
    };
    Ok(Row { body, checked, kind, at, abi, symbol, export: symbols.export, lits, intrinsic })
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
        Front2Refusal::Intrinsic { function } => {
            let name = function.rsplit("::").next().unwrap_or(function);
            format!("the intrinsic `{name}`, which the build's table does not name").into_boxed_str()
        }
        Front2Refusal::Literal { what, .. } => format!("a literal front2 does not read: {what}").into_boxed_str(),
        Front2Refusal::Attrs { why, .. } => format!("its attributes do not read: {why}").into_boxed_str(),
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
    left_out: Option<&mut Vec<(Box<str>, Box<str>), Global>>,
) -> Result<(), Front2Refusal> {
    let Some(out) = left_out else { return Err(refusal) };
    let class = class(&refusal);
    left.insert(key, class.clone());
    out.push((path, class));
    Ok(())
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
        assert_eq!(&*out[0].1, "a literal front2 does not read: this float literal");
        assert_eq!(left.get(&key).map(|c| &**c), Some("a literal front2 does not read: this float literal"));
        // A run has no list to file in: the same refusal stops it, named.
        let stopped = left_out_or_stop(key, Box::from("core::default::default"), literal(), &mut left, None);
        assert!(matches!(stopped, Err(Front2Refusal::Literal { what: "this float literal", .. })));
    }

    /// The causes the loop filed before keep their texts (the walk's lists group by them).
    ///
    /// Prints: `test front2::program::tabled::tests::the_filed_causes_keep_their_texts ... ok`
    #[test]
    fn the_filed_causes_keep_their_texts() {
        let f = || Box::<str>::from("a::b::ctpop_like");
        assert_eq!(&*class(&Front2Refusal::NoHead { function: f() }), "no head in the store (front2's)");
        assert_eq!(&*class(&Front2Refusal::NoBody { function: f() }), "no body (front2's)");
        assert_eq!(&*class(&Front2Refusal::Intrinsic { function: f() }), "the intrinsic `ctpop_like`, which the build's table does not name");
    }
}
