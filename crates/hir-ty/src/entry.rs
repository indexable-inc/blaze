//! The entry of a program of a blaze edition (docs/v2/plan/design/std.md, "The blaze edition:
//! opting in"): which parameters its `main` may take, and the module blaze appends to its
//! root to call it.
//!
//! **The rule** (`entry-takes-roots`): `main`'s parameters are roots the runtime makes, each
//! at most once, in the runtime's order: `&Cx`, then `Io` ([`Root::ALL`]). So `fn main()`,
//! `fn main(cx: &Cx)`, `fn main(io: Io)` and `fn main(cx: &Cx, io: Io)` are the entries, by
//! the rule and not by a list: a root added to [`Root`] is accepted where it stands in the
//! order. What `main` returns is the library's rule, the trait `bstd::entry::Outcome`,
//! which the appended call is checked against as any call is. In a crate of no blaze edition
//! `main` takes nothing, as in Rust, and one with parameters is refused naming the edition.
//!
//! [`takes`] is the rule as a pure function of what each parameter's type is ([`Param`]);
//! [`params`] reads those off a `main`'s signature, a root being the type its crate's `std`
//! (bstd) names `Cx` or `Io` at its root; [`module`] is the text appended for a `main` the
//! rule accepts, which hands it exactly the roots it takes.

use std::fmt;

use base_db::{Crate, Db};
use hir_def::{AdtId, CallableDefId, FunctionId, ModuleDefId, nameres::crate_def_map};
use hir_expand::name::Name;
use intern::{Symbol, sym};
use rustc_type_ir::inherent::{IntoKind as _, SliceLike as _};
use span::BlazeEdition;

use crate::{
    db::HirDatabase as _,
    next_solver::{Ty, TyKind},
};

/// The name of the rule, as every refusal says it.
pub const RULE: &str = "entry-takes-roots";

/// What the runtime makes for a program and can hand its `main`, in the order `main` takes
/// them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Root {
    /// The root context, by shared reference: `&Cx`.
    Cx,
    /// The program's world, by value: `Io`.
    Io,
}

impl Root {
    /// Every root, in the order a `main` takes them.
    pub const ALL: [Root; 2] = [Root::Cx, Root::Io];

    /// The type a parameter that takes it is written with.
    pub const fn written(self) -> &'static str {
        match self {
            Root::Cx => "&Cx",
            Root::Io => "Io",
        }
    }

    /// The name of the type at the root of the crate's `std`.
    const fn type_name(self) -> &'static str {
        match self {
            Root::Cx => "Cx",
            Root::Io => "Io",
        }
    }

    /// The appended call's name for it.
    const fn arg(self) -> &'static str {
        match self {
            Root::Cx => "cx",
            Root::Io => "io",
        }
    }
}

/// What a parameter of `main` is, by its type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Param {
    /// It takes this root: its type is the root's, as [`Root::written`].
    Root(Root),
    /// Any other type.
    Other,
}

/// The roots a `main` takes: a subset of [`Root::ALL`], which is also their order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Takes {
    pub cx: bool,
    pub io: bool,
}

impl Takes {
    pub const fn has(self, root: Root) -> bool {
        match root {
            Root::Cx => self.cx,
            Root::Io => self.io,
        }
    }

    fn with(self, root: Root) -> Takes {
        match root {
            Root::Cx => Takes { cx: true, ..self },
            Root::Io => Takes { io: true, ..self },
        }
    }
}

/// Why a `main` is no entry, parameters counted from 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The crate is of no blaze edition and its `main` takes `params` parameters: in Rust
    /// `main` takes none.
    NeedsEdition { params: usize },
    /// The parameter at `at` is no root.
    NotARoot { at: usize },
    /// The parameter at `at` takes `root`, which an earlier one took.
    Twice { root: Root, at: usize },
    /// The parameter at `at` takes `root`, which comes before `after`, taken earlier.
    OutOfOrder { root: Root, at: usize, after: Root },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let latest = BlazeEdition::LATEST.manifest();
        match *self {
            Refused::NeedsEdition { params } => write!(
                f,
                "`main` takes {params} parameter{}, which needs the blaze edition: in Rust `main` \
                 takes none. Add `edition = \"{latest}\"` to `[package]` in Cargo.toml and `main` \
                 may take `cx: &Cx` and `io: Io` (rule {RULE})",
                if params == 1 { "" } else { "s" },
            ),
            Refused::NotARoot { at } => write!(
                f,
                "parameter {} of `main` is not something the runtime makes: `main` of the blaze \
                 edition takes `cx: &Cx`, `io: Io`, both in that order, or nothing (rule {RULE})",
                at + 1,
            ),
            Refused::Twice { root, at } => write!(
                f,
                "parameter {} of `main` takes `{}` a second time: the runtime makes one, and \
                 `main` takes each root at most once (rule {RULE})",
                at + 1,
                root.written(),
            ),
            Refused::OutOfOrder { root, at, after } => write!(
                f,
                "parameter {} of `main` takes `{}` after `{}`: `main` takes its roots in the \
                 runtime's order, `{}` first (rule {RULE})",
                at + 1,
                root.written(),
                after.written(),
                root.written(),
            ),
        }
    }
}

/// The rule: the roots a `main` with the parameters `params` takes, in a crate of the blaze
/// edition `edition` (none: Rust's), or why it is no entry.
pub fn takes(edition: Option<BlazeEdition>, params: &[Param]) -> Result<Takes, Refused> {
    if edition.is_none() {
        return match params {
            [] => Ok(Takes::default()),
            _ => Err(Refused::NeedsEdition { params: params.len() }),
        };
    }
    let mut takes = Takes::default();
    let mut last: Option<Root> = None;
    for (at, param) in params.iter().enumerate() {
        let Param::Root(root) = *param else { return Err(Refused::NotARoot { at }) };
        match last {
            Some(before) if before == root => return Err(Refused::Twice { root, at }),
            Some(before) if before > root => {
                return Err(Refused::OutOfOrder { root, at, after: before });
            }
            _ => {}
        }
        takes = takes.with(root);
        last = Some(root);
    }
    Ok(takes)
}

/// What [`module`] starts with, after the text of the root file.
pub const MARK: &str = "\n// blaze's entry of the blaze edition, which is not in the file on disk.\n";

/// The module appended to the root of a program whose `main` takes `takes`
/// ([`BlazeEdition::ENTRY_MODULE`]): its `main` is the program's entry, and calls the crate's
/// own with the roots it takes, through `std::entry::run` (bstd's), which makes them. Plain
/// code a program could have written; nothing in it is the compiler's.
pub fn module(takes: Takes) -> String {
    let binds: Vec<String> = Root::ALL
        .iter()
        .map(|&root| match takes.has(root) {
            true => root.arg().to_owned(),
            false => format!("_{}", root.arg()),
        })
        .collect();
    let hands: Vec<&str> =
        Root::ALL.iter().filter(|&&root| takes.has(root)).map(|root| root.arg()).collect();
    format!(
        "{MARK}#[doc(hidden)]\n#[allow(warnings, clippy::all)]\npub mod {module} {{\n    pub fn \
         main() -> ::std::entry::Exit {{\n        ::std::entry::run(|{binds}| \
         crate::main({hands}))\n    }}\n}}\n",
        module = BlazeEdition::ENTRY_MODULE,
        binds = binds.join(", "),
        hands = hands.join(", "),
    )
}

/// The types the `std` of `krate` (the dependency named [`BlazeEdition::STD`]) names `Cx` and
/// `Io` at its root; none when it has no such dependency or the names are not types there.
fn roots_of(db: &Db, krate: Crate) -> Option<[AdtId; 2]> {
    let std = krate
        .data(db)
        .dependencies
        .iter()
        .rev()
        .find(|dep| dep.as_name().as_str() == BlazeEdition::STD)?
        .crate_id;
    let def_map = crate_def_map(db, std);
    let scope = def_map.module(db, def_map.crate_root(db)).scope(db);
    let adt = |root: Root| {
        let named = scope.get(&Name::new_symbol_root(Symbol::intern(root.type_name())));
        match named.types.map(|it| it.def) {
            Some(ModuleDefId::AdtId(adt)) => Some(adt),
            _ => None,
        }
    };
    Some([adt(Root::Cx)?, adt(Root::Io)?])
}

fn param<'db>(roots: Option<[AdtId; 2]>, ty: Ty<'db>) -> Param {
    let Some([cx, io]) = roots else { return Param::Other };
    match ty.kind() {
        TyKind::Ref(_, inner, rustc_ast_ir::Mutability::Not) => match inner.kind() {
            TyKind::Adt(def, _) if def.def_id() == cx => Param::Root(Root::Cx),
            _ => Param::Other,
        },
        TyKind::Adt(def, _) if def.def_id() == io => Param::Root(Root::Io),
        _ => Param::Other,
    }
}

/// What each parameter of `main`, a function of `krate`, is.
pub fn params(db: &Db, krate: Crate, main: FunctionId) -> Vec<Param> {
    let roots = roots_of(db, krate);
    let sig = db.callable_item_signature(CallableDefId::FunctionId(main));
    let sig = sig.instantiate_identity().skip_binder();
    // alloc: once per program, its `main`'s few parameters
    sig.inputs().as_slice().iter().map(|&ty| param(roots, ty)).collect()
}

/// The function the root of `krate` names `main`, if it names one.
pub fn main_of(db: &Db, krate: Crate) -> Option<FunctionId> {
    let def_map = crate_def_map(db, krate);
    let main = def_map
        .module(db, def_map.crate_root(db))
        .scope(db)
        .get(&Name::new_symbol_root(sym::main));
    match main.values.map(|it| it.def) {
        Some(ModuleDefId::FunctionId(main)) => Some(main),
        _ => None,
    }
}

/// The rule asked of `krate`'s own `main`: `None` when its root names no function `main`
/// (rustc's E0601, reported as such).
pub fn checked(db: &Db, krate: Crate) -> Option<Result<Takes, Refused>> {
    let main = main_of(db, krate)?;
    Some(takes(krate.blaze_edition(db), &params(db, krate, main)))
}

/// The module to append to the root of `krate`, a program: [`module`] for a crate of a blaze
/// edition whose `main` the rule accepts. `None` for a crate of no blaze edition, which has
/// nothing appended, and for one whose `main` is missing or refused: the crate's checks
/// report that, and its build names the missing entry.
pub fn appended(db: &Db, krate: Crate) -> Option<String> {
    krate.blaze_edition(db)?;
    checked(db, krate)?.ok().map(module)
}

#[cfg(test)]
mod tests {
    use span::BlazeEdition;

    use super::{Param, Refused, Root, Takes, module, takes};

    const CX: Param = Param::Root(Root::Cx);
    const IO: Param = Param::Root(Root::Io);
    const EDITION: Option<BlazeEdition> = Some(BlazeEdition::Blaze2026_11);

    /// Every list of up to three parameters over {`&Cx`, `Io`, other}, each judged against the
    /// rule said another way: accepted exactly when it is a strictly increasing run of roots.
    /// The invariant: `takes` accepts a `main` if and only if the runtime can hand it each
    /// parameter, once, in its order.
    #[test]
    fn the_entry_rule_accepts_exactly_the_increasing_runs_of_roots() {
        let kinds = [CX, IO, Param::Other];
        let mut lists: Vec<Vec<Param>> = vec![vec![]];
        for len in 1..=3usize {
            for n in 0..kinds.len().pow(len as u32) {
                lists.push((0..len).map(|at| kinds[n / kinds.len().pow(at as u32) % kinds.len()]).collect());
            }
        }
        assert_eq!(lists.len(), 1 + 3 + 9 + 27);
        let mut accepted = Vec::new();
        for list in &lists {
            let model = model(list);
            let got = takes(EDITION, list);
            assert_eq!(got.is_ok(), model.is_some(), "{list:?}: {got:?}");
            if let (Ok(got), Some(model)) = (got, model) {
                assert_eq!(got, model, "{list:?}");
                accepted.push(list.clone());
            }
        }
        assert_eq!(accepted, [vec![], vec![CX], vec![IO], vec![CX, IO]], "the four entries, by the rule");
    }

    /// The reference: each parameter a root, later than the one before it.
    fn model(list: &[Param]) -> Option<Takes> {
        let mut roots = Vec::new();
        for param in list {
            let Param::Root(root) = param else { return None };
            roots.push(*root);
        }
        roots.windows(2).all(|pair| pair[0] < pair[1]).then(|| Takes {
            cx: roots.contains(&Root::Cx),
            io: roots.contains(&Root::Io),
        })
    }

    /// The control (skill `testing`, planted bug): a rule that forgot the order, accepting any
    /// list of roots, differs from the model on `(io, cx)` and on a root taken twice, so the
    /// test above is not one that passes whatever `takes` does.
    #[test]
    fn a_rule_without_the_order_is_caught() {
        let planted = |list: &[Param]| list.iter().all(|it| matches!(it, Param::Root(_)));
        let caught: Vec<&[Param]> = [&[IO, CX][..], &[CX, CX], &[IO, IO], &[CX, IO, CX]]
            .into_iter()
            .filter(|list| planted(list) != model(list).is_some())
            .collect();
        assert_eq!(caught.len(), 4, "the model refuses what the planted rule lets in");
        for list in caught {
            assert!(takes(EDITION, list).is_err(), "{list:?}");
        }
    }

    /// Each refusal says which parameter and why, and names the rule.
    #[test]
    fn each_refusal_names_its_parameter_and_the_rule() {
        let cases: [(&[Param], Refused, &str); 4] = [
            (&[Param::Other], Refused::NotARoot { at: 0 }, "parameter 1"),
            (&[CX, Param::Other], Refused::NotARoot { at: 1 }, "parameter 2"),
            (&[IO, IO], Refused::Twice { root: Root::Io, at: 1 }, "`Io` a second time"),
            (
                &[IO, CX],
                Refused::OutOfOrder { root: Root::Cx, at: 1, after: Root::Io },
                "`&Cx` after `Io`",
            ),
        ];
        for (list, refused, says) in cases {
            assert_eq!(takes(EDITION, list), Err(refused), "{list:?}");
            let said = refused.to_string();
            assert!(said.contains(says) && said.contains(super::RULE), "{said}");
        }
    }

    /// A crate of no blaze edition: `main()` is Rust's and nothing changes; a `main` with
    /// parameters, roots or not, is refused naming the edition and the line to add.
    #[test]
    fn without_the_edition_main_takes_nothing() {
        assert_eq!(takes(None, &[]), Ok(Takes::default()));
        for list in [&[IO][..], &[CX, IO], &[Param::Other]] {
            let refused = takes(None, list).unwrap_err();
            assert_eq!(refused, Refused::NeedsEdition { params: list.len() });
            let said = refused.to_string();
            assert!(said.contains("edition = \"blaze-2026.11\"") && said.contains("needs the blaze edition"), "{said}");
        }
    }

    /// The appended module hands `main` exactly the roots it takes, in order, and is the same
    /// text each time (it is part of the root's content, so of the crate's key).
    #[test]
    fn the_appended_module_hands_main_what_it_takes() {
        let call = |cx, io| {
            let text = module(Takes { cx, io });
            assert!(text.starts_with(super::MARK) && text.contains("pub mod __blaze_entry {"), "{text}");
            assert!(text.contains("pub fn main() -> ::std::entry::Exit {"), "{text}");
            let from = text.find("::std::entry::run(").unwrap();
            text[from..text[from..].find('\n').unwrap() + from].to_owned()
        };
        assert_eq!(call(true, true), "::std::entry::run(|cx, io| crate::main(cx, io))");
        assert_eq!(call(false, true), "::std::entry::run(|_cx, io| crate::main(io))");
        assert_eq!(call(true, false), "::std::entry::run(|cx, _io| crate::main(cx))");
        assert_eq!(call(false, false), "::std::entry::run(|_cx, _io| crate::main())");
        assert_eq!(module(Takes { cx: true, io: true }), module(Takes { cx: true, io: true }));
    }
}
