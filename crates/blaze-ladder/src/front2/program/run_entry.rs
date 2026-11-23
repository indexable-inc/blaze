//! The entry of `blaze run`'s record walk, as a key, from text and the names source alone (D106
//! piece Z, item 3): what the command hands `run::make_one::run_records` as `entry`, before any
//! body is checked and with no world solved.
//!
//! WHO computes it, and from what: the command (PIPELINE's `blaze run`), once per run, from
//! - the program crate's (crate 0's) ROOT TEXT and its cfg atoms and edition: the root's own
//!   attributes (`#![no_core]`, `#![no_std]`), read by blaze-scope's `file_attrs` over the
//!   root's outline exactly as the solve reads them ([`RootStart::of_root`]);
//! - the crate's `Implicit` from the unit graph (`StdIs`: Rust's std or bstd), part of the
//!   crate's input, so known without names;
//! - crate 0's ITEMS RECORD through the names source ([`run_entry`]): its fn rows, and for each
//!   its own text's attribute rows (`#[no_mangle]`, `#[export_name]`): THE ENTRY RULE is
//!   program.rs [`exported_entry`] (one home, generic over the source), then [`RootStart`].
//!   Only crate 0's record is opened: no library crate's names are read for the entry.
//!
//! D104 (STEP A) changed what a blaze-edition binary's root holds: a top-level `fn main` with
//! no attribute macro written is the call of `::std::main` (`CallSrc::implicit`), whose
//! expansion is a row of crate 0's items and exports bstd's hosted entry
//! ([`HOSTED_START`]). So a blaze-edition program's entry is ALWAYS an exported fn, found by
//! the rule's first arm; a blaze-edition root with none is refused by name
//! ([`EntryRefusal::NoHostedStart`]), never started at its `main` directly (that would skip
//! the grants `bstd::main` makes).
//!
//! A Rust-edition program with std starts through `#[lang = "start"]`: `lang_start::<T>` with
//! `main` as its argument, called by the C entry shim. [`run_entry`] names its `main`
//! ([`EntryHow::LangStart`]); the walk's roots (the shim, `lang_start::<T>` and `main` as
//! instances) are crates/blaze-ladder/src/run/special.rs `roots`, the one home of the run's
//! special records (D124). `#![no_core]` / `#![no_std]` roots start at their `fn main`
//! directly (start.rs `Entry2::Direct`), as the solved front does.

use blaze_own::Alloc;
use blaze_surface::{Edition, ItemKind};
use blaze_typeck::ItemKey;
use blaze_unit_graph::StdIs;

use super::{Front2Refusal, LibKey, exported_entry};
use crate::front2::keys::checker_of;
use crate::front2::source::NamesSource;
use crate::front2::start::Entry2;

/// How the program crate starts when it exports no entry of its own: decided from its root's
/// attributes and its `Implicit`, never from names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootStart {
    /// A blaze-edition crate: `bstd::main`'s expansion exports the hosted entry (D104).
    Hosted,
    /// `#![no_core]` or `#![no_std]`: the root's `fn main`, called directly.
    Direct,
    /// A Rust-edition crate with Rust's std: through `#[lang = "start"]`.
    LangStart,
}

impl RootStart {
    /// The rule of start.rs `through_lang_start`, from the facts a command has before names:
    /// `std` is the crate's `Implicit::std`; `no_core`/`no_std` its root's own attributes.
    pub fn of(std: StdIs, no_core: bool, no_std: bool) -> RootStart {
        if no_core || no_std {
            return RootStart::Direct;
        }
        match std {
            StdIs::Rust => RootStart::LangStart,
            StdIs::Bstd(_) => RootStart::Hosted,
        }
    }

    /// [`RootStart::of`] with the root's attributes read from its TEXT: its outline's file
    /// attributes under the crate's cfg (blaze-scope `file_attrs`, the solve's own reader, so
    /// the two agree by construction). `atoms` are the crate's cfg atoms (the items record's
    /// `cfg_atoms`, or the crate input's), `alloc` holds the outline and is dropped by the
    /// caller.
    pub fn of_root(std: StdIs, root_text: &str, edition: Edition, atoms: &[(&str, Option<&str>)], alloc: Alloc<'_>) -> RootStart {
        let outline = blaze_file_items::outline_auto_extensions(root_text, edition, blaze_file_items::Bodies::Skipped, alloc);
        let cfg = blaze_scope::task::Cfg::from_atoms(atoms, alloc);
        let attrs = blaze_scope::task::file_attrs(&cfg, &outline, root_text, alloc);
        RootStart::of(std, attrs.no_core, attrs.no_std)
    }

    /// [`RootStart::of`] over any names source: crate 0's `Implicit::std` and its root's own
    /// `#![no_core]`/`#![no_std]` as the solve read them (blaze-scope `CrateNames::root_attrs`,
    /// by the same `file_attrs` [`RootStart::of_root`] reads), so both fronts decide by one rule
    /// from the same facts. `None`: the source holds no crate, or did not answer one of the two
    /// facts (over records the error is in its log).
    ///
    /// OWED by front2 source.rs (lane M): `NamesSource::root_attrs(self, krate: usize) ->
    /// Option<blaze_scope::graph::RootAttrs>` (over the solved names `Some(names.crates()
    /// .get(krate)?.root_attrs)`; over records, the names record's copy of the same fact).
    pub fn of_names<'n, S: NamesSource<'n>>(names: S) -> Option<RootStart> {
        let std = names.std_is(0)?;
        // OWED: `NamesSource::root_attrs` (above).
        let attrs = names.root_attrs(0)?;
        Some(RootStart::of(std, attrs.no_core, attrs.no_std))
    }

    /// What a program that exports no entry of its own starts through: THE rule both fronts
    /// take. A blaze-edition root is refused ([`HostedStartMissing`], D104), never started at its
    /// `main` directly; what is left is a Rust start ([`Unhosted`]).
    pub fn unexported(self) -> Result<Unhosted, HostedStartMissing> {
        match self {
            RootStart::Hosted => Err(HostedStartMissing),
            RootStart::LangStart => Ok(Unhosted::LangStart),
            RootStart::Direct => Ok(Unhosted::Direct),
        }
    }
}

/// A start that is not hosted: what [`RootStart::unexported`] leaves, so a value of it is the
/// proof that no blaze-edition root was started past its missing hosted entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unhosted {
    /// A Rust-edition root with Rust's std: through `#[lang = "start"]` (the solved front reads
    /// which crates are linked: start.rs `linked_lang_start`).
    LangStart,
    /// `#![no_core]` or `#![no_std]`: the root's `fn main`, called directly.
    Direct,
}

/// A blaze-edition program crate exports no hosted entry ([`crate::front::HOSTED_START`]):
/// D104's `bstd::main` expansion of its root's `fn main` should have made it. The one refusal
/// both fronts give: the record walk as [`EntryRefusal::NoHostedStart`], the solved front as
/// `Front2Refusal::NoHostedStart`; never a start at `main` directly (that would skip the
/// grants `bstd::main` makes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostedStartMissing;

impl core::fmt::Display for HostedStartMissing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "a blaze-edition program exports no `{}` (D104: its root's `fn main` is expanded by `bstd::main`, which exports it)", crate::front::HOSTED_START)
    }
}

/// The run's entry: the fn the record walk starts at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunEntry {
    /// Its checker key: `run_records`'s `entry`.
    pub key: ItemKey,
    /// Its key in the names library (crate 0's items record).
    pub lib: LibKey,
    pub how: EntryHow,
}

/// Which arm of the entry rule found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryHow {
    /// Exported as bstd's hosted entry (`HOSTED_START`).
    Hosted,
    /// Exported as `main`: the C entry the platform calls.
    CMain,
    /// The root's `fn main`, called directly (`#![no_core]`/`#![no_std]`).
    Direct,
    /// A Rust-edition program with Rust's std: `key` is the root's `fn main`, run by
    /// `lang_start::<T>` through the C entry shim. The walk's roots and the shim are
    /// run/special.rs `roots`' (the one home of the special records); this names `main` only.
    LangStart,
}

/// Why the run has no entry key.
#[derive(Debug)]
pub enum EntryRefusal {
    /// The entry rule itself refused (two fns under one entry symbol, an export's attributes
    /// that do not read, an unanswered fact of crate 0's record).
    Rule(Front2Refusal),
    /// A blaze-edition program exports no hosted entry: D104's `bstd::main` expansion should
    /// have made it from the root's `fn main`.
    NoHostedStart,
    /// A `#![no_core]`/`#![no_std]` root with no `fn main` at its top (Reference,
    /// crates-and-source-files.main: "a binary crate must have a function named `main`").
    NoMain,
    /// Crate 0's items record did not list its items (the error is in the source's log).
    NoRows,
}

impl core::fmt::Display for EntryRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EntryRefusal::Rule(r) => write!(f, "the run's entry: {r}"),
            EntryRefusal::NoHostedStart => write!(f, "the run's entry: {HostedStartMissing}"),
            EntryRefusal::NoMain => write!(f, "the run's entry: no `fn main` at the top of the crate root (the Reference, crates-and-source-files.main)"),
            EntryRefusal::NoRows => write!(f, "the run's entry: the program crate's items record did not list its items"),
        }
    }
}

/// THE RUN'S ENTRY over any names source: the entry rule ([`exported_entry`]), then
/// `start`. Reads only crate 0's items record (its rows, its fns' texts).
pub fn run_entry<'n, S: NamesSource<'n>>(names: S, start: RootStart) -> Result<RunEntry, EntryRefusal> {
    match exported_entry(names).map_err(EntryRefusal::Rule)? {
        Some(Entry2::Hosted(lib)) => return Ok(RunEntry { key: checker_of(lib), lib, how: EntryHow::Hosted }),
        Some(Entry2::CMain(lib)) => return Ok(RunEntry { key: checker_of(lib), lib, how: EntryHow::CMain }),
        // `exported_entry` answers only the two exported arms.
        Some(Entry2::Rust(_) | Entry2::Direct(_)) | None => {}
    }
    let how = match start.unexported().map_err(|HostedStartMissing| EntryRefusal::NoHostedStart)? {
        Unhosted::LangStart => EntryHow::LangStart,
        Unhosted::Direct => EntryHow::Direct,
    };
    let lib = root_main(names)?;
    Ok(RunEntry { key: checker_of(lib), lib, how })
}

/// The fn `main` declared at the top of crate 0's root module: by its row (name, home, kind),
/// in key order. Two such rows cannot be (the solve refuses a name defined twice in one
/// namespace); a `use` of another fn as `main` is not this row and is refused as no main.
pub fn root_main<'n, S: NamesSource<'n>>(names: S) -> Result<LibKey, EntryRefusal> {
    let root = names.root_module(0).ok_or(EntryRefusal::NoRows)?;
    let mut found: Option<LibKey> = None;
    let listed = names.each_item(0, &mut |key, module| {
        if found.is_none() && module == root && names.name_of(key) == Some("main") && names.kind_of_item(key) == Some(ItemKind::Function) && names.member_owner(key).is_none() {
            found = Some(key);
        }
    });
    if !listed {
        return Err(EntryRefusal::NoRows);
    }
    found.ok_or(EntryRefusal::NoMain)
}
