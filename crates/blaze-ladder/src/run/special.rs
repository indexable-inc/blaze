//! The run's ROOTS and its SPECIAL RECORDS (docs/v2/plan/design/run.md, "The cut to the lazy
//! parent (D124)", "The entry (ladder)"): the one home of what `blaze run`'s parent needs before
//! its first `Need` and of every code record it serves that is not an item's instance.
//!
//! THE ROOTS ([`roots`]): from the names source alone (generic over [`NamesSource`], so the
//! parent asks it over the names RECORDS with nothing solved), by the one start rule
//! (front2 program.rs [`start_of`]):
//! - the ENTRY's symbol and its launcher FORM ([`EntryForm`], printed by [`EntryLine`] as
//!   `<form>:<64 hex>`, the launcher's `BLAZE_LAUNCH_ENTRY`, run_mode.rs `Start::parse`);
//! - the ROOT INSTANCES, each `(tabled fn, instance name)` as [`crate::body_ids::Walk::root`]
//!   takes it: what the walk reaches first.
//!
//! By start:
//!
//! | start                          | entry                     | form     | root rows                  |
//! |--------------------------------|---------------------------|----------|----------------------------|
//! | bstd's hosted entry (exported) | `bstd_hosted_start`       | `hosted` | it                         |
//! | a C `main` the program exports | that fn                   | `c-main` | it                         |
//! | Rust's std (`#[lang = "start"]`)| the C entry SHIM (ours)  | `c-main` | `lang_start::<T>`, `main`  |
//! | `main` called directly         | the root's `fn main`      | `unit`   | `main`                     |
//!
//! `T` is `main`'s return type, read from `main`'s HEAD (start.rs [`rust_start`]); that is the
//! one head read, and no body of any fn is read.
//!
//! THE SPECIAL RECORDS ([`Special`]): the shim (one a Rust program, made here from the roots'
//! two symbols by blaze-build's `start::c_main_shim`), the thread-local getter and the
//! thread-local ender (one a program each, made by blaze-build's `tls::index`). A symbol the
//! walk's table does not hold is one of these ([`special_of`]) or is refused by name by the
//! parent. Each is made once ([`make_special`]): found by its key on a repeat run, else made
//! and put as a PLAIN entry (code_records.rs `CodeStore`: no read list, never stale). Its key is
//! code_records.rs `code_key` over its parts and the [`Builder`] (target, tier, settings,
//! toolchain, the code builder's identity, the edition, the form): everything that decides the
//! bytes (rule 21). The shim's parts hold both root symbols, their tabled fns, its two
//! reference places and `SIGPIPE_DEFAULT`; the getter's and the ender's are code_records.rs
//! `getter_parts` and tls_index.rs `ender_parts`.
//!
//! The record walk of the old path (run.rs `build_from`, through `RecordRun::getter` and
//! `RecordRun::ender`) makes the getter and the ender through [`make_special`]: one copy.

use core::fmt;
use std::alloc::Global;

use blaze_code::record::Symbol;
use blaze_emit::Isa;
use blaze_ids::{ContentId, Hasher};
use blaze_lir::func::RefKind;
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::{ItemKey, List, PTys};

use crate::body_ids::DeclaredList;
use crate::code_records::{
    Builder, Callee, CodeKey, CodeRecordError, CodeStore, Declared, GETTER_NAME, InstanceNames, KeyParts, code_key, getter_parts, make_entry, make_getter_entry,
};
use crate::front2::heads::Store;
use crate::front2::keys::{Keys, checker_of};
use crate::front2::program::run_entry::{EntryRefusal, root_main};
use crate::front2::program::{Front2Refusal, StartOf, start_of};
use crate::front2::source::NamesSource;
use crate::front2::start::{Entry2, rust_start};
use crate::symbols::{SymbolError, encode_instance, fn_symbol_of_name, tls_getter_symbol};
use crate::tls_index::{ENDER_NAME, ender_parts, make_ender_entry, tls_ender_symbol};

/// The domain of the shim's symbol: one shim a (lang_start, main) pair.
const SHIM_SYMBOL_DOMAIN: &str = "blaze-ladder C entry shim symbol 1";

/// The domain of the shim's body digest: stepped when blaze-build's `start::c_main_shim`
/// changes what it builds.
const SHIM_DOMAIN: &str = "blaze-ladder C entry shim body 1";

/// How a walk's `Met` names the C entry shim.
pub const SHIM_NAME: &str = "the C entry shim";

/// The shim's reference places (blaze-build tests/c_main_shim.rs: `lang_start::<T>` at 0,
/// `main` at 1).
const LANG_START_PLACE: u32 = 0;
const MAIN_PLACE: u32 = 1;

/// The pointer width the shim is built at: the emitter makes AArch64 code only
/// (code_records.rs `triple`), whose pointers are 64 bits; the target is in every key.
const PTR_BITS: u32 = 64;

/// How the launcher starts the entry (blaze-launch-rt run_mode.rs `EntryForm`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryForm {
    /// `fn()`, called with nothing: a root's `fn main` called directly.
    Unit,
    /// `extern "C" fn(argc: i32, argv: *const *const c_char) -> i32`: a C `main` the program
    /// exports, or the shim of a Rust program with std.
    CMain,
    /// bstd's hosted entry `extern "C" fn(table: *const HostTable) -> i32`: the child builds
    /// the host table (lane H's launcher piece).
    Hosted,
}

impl EntryForm {
    /// The form's word in the launcher's line.
    pub const fn word(self) -> &'static str {
        match self {
            EntryForm::Unit => "unit",
            EntryForm::CMain => "c-main",
            EntryForm::Hosted => "hosted",
        }
    }
}

/// The launcher's `BLAZE_LAUNCH_ENTRY`: `<form>:<64 lowercase hex digits of the symbol>`. THE
/// one home of that line: blaze-cli own_process.rs `c_main_line` and run_parent.rs
/// `entry_line` print it through this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryLine {
    pub form: EntryForm,
    pub symbol: Symbol,
}

impl EntryLine {
    /// The line's length in bytes: the form's word, `:` and 64 digits.
    pub const fn len(&self) -> usize {
        self.form.word().len() + 1 + 64
    }

    /// Never empty: kept beside [`EntryLine::len`].
    pub const fn is_empty(&self) -> bool {
        false
    }
}

impl fmt::Display for EntryLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.form.word())?;
        f.write_str(":")?;
        self.symbol.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Why the run has no roots: each a refusal of the start rule or of one root, by name.
#[derive(Debug)]
pub enum RootsError {
    /// The start rule refused (program.rs `start_of`), or `main`'s head did (start.rs
    /// `rust_start`: parameters, generics, an ABI, `unsafe`).
    Start(Front2Refusal),
    /// The root's `fn main` was not found in crate 0's rows (run_entry.rs `root_main`).
    Entry(EntryRefusal),
    /// A root's instance name did not encode (symbols.rs `encode_instance`): `what` says which.
    Symbol { what: &'static str, error: SymbolError },
    /// `lang_start`'s argument list `[T]` did not intern in the head store's table.
    Types(blaze_typeck::CheckError),
}

impl fmt::Display for RootsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootsError::Start(refusal) => write!(f, "the run's roots: {refusal}"),
            RootsError::Entry(refusal) => write!(f, "the run's roots: {refusal}"),
            RootsError::Symbol { what, error } => write!(f, "the run's roots: {what}'s instance name: {error}"),
            RootsError::Types(error) => write!(f, "the run's roots: `lang_start`'s argument list `[T]`: {error:?}"),
        }
    }
}

impl core::error::Error for RootsError {}

/// One root instance: its tabled fn (the checker key whose body or signature builds it), its
/// instance name (symbols.rs `encode_instance`) and its symbol (the name's hash).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RootRow<'r> {
    pub tabled: ItemKey,
    pub name: &'r [u8],
    pub symbol: Symbol,
}

/// The shim's two callees: `lang_start::<T>` and `main`, both root rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shim<'r> {
    pub lang_start: RootRow<'r>,
    pub main: RootRow<'r>,
}

/// The run's roots ([`roots`]; the module doc's table).
pub struct Roots<'a> {
    /// What the start rule answered.
    pub start: StartOf,
    /// How the launcher starts [`Roots::entry`].
    pub form: EntryForm,
    /// The symbol the launcher is told: the shim's for a Rust program with std, else the root
    /// instance's (row 0's).
    pub entry: Symbol,
    rows: DeclaredList<'a>,
    symbols: AVec<'a, Symbol>,
    /// Whether the entry is the shim, rows 0 and 1 its callees (`lang_start::<T>`, `main`).
    shim: bool,
}

impl<'a> Roots<'a> {
    /// The launcher's line for the entry.
    pub fn line(&self) -> EntryLine {
        EntryLine { form: self.form, symbol: self.entry }
    }

    /// How many root instances.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Root instance `i`, as [`crate::body_ids::Walk::root`] takes it: `(tabled fn, name)`.
    pub fn row(&self, i: usize) -> (ItemKey, &[u8]) {
        self.rows.row(i)
    }

    /// Root instance `i` with its symbol.
    pub fn root(&self, i: usize) -> RootRow<'_> {
        let (tabled, name) = self.rows.row(i);
        RootRow { tabled, name, symbol: self.symbols[i] }
    }

    /// The shim's callees, when the entry is the shim.
    pub fn shim(&self) -> Option<Shim<'_>> {
        self.shim.then(|| Shim { lang_start: self.root(0), main: self.root(1) })
    }
}

/// THE RUN'S ROOTS over any names source (the module doc). Reads, through `names`:
/// - the start rule's facts (program.rs `start_of`): crate 0's fn rows and their export
///   attributes, crate 0's `Implicit::std` and root attributes, the crates crate 0 links, their
///   lang rows and which is Rust's `std`;
/// - for a direct or a Rust start, crate 0's rows to find the root's `fn main` (run_entry.rs
///   `root_main`);
/// - for a Rust start only, `main`'s HEAD (start.rs `rust_start`), lowered in a head store made
///   here in `frame` with `tables` (the store's type table; dropped with it).
///
/// No body. `alloc` holds the roots (their names and symbols) and the start rule's lists.
pub fn roots<'n, 'a, S: NamesSource<'n>>(names: S, frame: &'n blaze_mem::Frame, tables: Alloc<'static>, alloc: Alloc<'a>) -> Result<Roots<'a>, RootsError> {
    let start = start_of(names, alloc).map_err(RootsError::Start)?;
    // The `sized` trait is the implied bound of a head's own type parameters; `main` declares
    // none (`rust_start` refuses one that does), so its head needs no lookup of it.
    let mut store = Store::new(None, frame, tables);
    let mut table = RootTable { rows: DeclaredList::new_in(alloc), symbols: Vec::new_in(alloc), name: Vec::new_in(alloc), alloc };
    let (form, entry, shim) = match start {
        StartOf::Exported(Entry2::Hosted(lib)) => (EntryForm::Hosted, table.push("the hosted entry", checker_of(lib), List::EMPTY, store.tys.view())?, false),
        StartOf::Exported(Entry2::CMain(lib)) => (EntryForm::CMain, table.push("the exported `main`", checker_of(lib), List::EMPTY, store.tys.view())?, false),
        // `exported_entry` answers only the two exported arms; the other two are read as what
        // they mean, so no arm is a guess.
        StartOf::Exported(Entry2::Direct(main)) => (EntryForm::Unit, table.push("`main`", checker_of(main), List::EMPTY, store.tys.view())?, false),
        StartOf::Exported(Entry2::Rust(s)) => (EntryForm::CMain, table.rust(names, s.lang_start, s.main, &mut store)?, true),
        StartOf::LangStart(lang_start) => {
            let main = root_main(names).map_err(RootsError::Entry)?;
            (EntryForm::CMain, table.rust(names, lang_start, main, &mut store)?, true)
        }
        StartOf::Direct => {
            let main = root_main(names).map_err(RootsError::Entry)?;
            (EntryForm::Unit, table.push("`main`", checker_of(main), List::EMPTY, store.tys.view())?, false)
        }
    };
    Ok(Roots { start, form, entry, rows: table.rows, symbols: table.symbols, shim })
}

/// The root rows as [`roots`] fills them.
struct RootTable<'a> {
    rows: DeclaredList<'a>,
    symbols: AVec<'a, Symbol>,
    /// The instance name being encoded, reused.
    name: AVec<'a, u8>,
    alloc: Alloc<'a>,
}

impl<'a> RootTable<'a> {
    /// The instance `item` at `args` (ground in `ground`) as the next row: its symbol. `what`
    /// names it in a refusal.
    fn push(&mut self, what: &'static str, item: ItemKey, args: List, ground: PTys<'_>) -> Result<Symbol, RootsError> {
        self.name.clear();
        encode_instance(item, args, ground, self.alloc, &mut self.name).map_err(|error| RootsError::Symbol { what, error })?;
        let symbol = fn_symbol_of_name(&self.name);
        self.rows.push(item, &self.name);
        self.symbols.push(symbol);
        Ok(symbol)
    }

    /// The roots of a Rust start: `lang_start::<T>` (row 0) and `main` (row 1), `T` from
    /// `main`'s head, lowered in `store`; the shim's symbol.
    fn rust<'n, S: NamesSource<'n>>(&mut self, names: S, lang_start: blaze_id::ItemKey, main: blaze_id::ItemKey, store: &mut Store<'n>) -> Result<Symbol, RootsError> {
        let mut keys = Keys::new();
        let start = rust_start(names, lang_start, main, store, &mut keys).map_err(RootsError::Start)?;
        let args = store.tys.intern_list(&[start.returns]).map_err(RootsError::Types)?;
        let lang_start = self.push("`lang_start::<T>`", checker_of(start.lang_start), args, store.tys.view())?;
        let main = self.push("`main`", checker_of(start.main), List::EMPTY, store.tys.view())?;
        Ok(shim_symbol(lang_start, main))
    }
}

/// The shim's symbol: one shim a pair of callees.
pub fn shim_symbol(lang_start: Symbol, main: Symbol) -> Symbol {
    let mut both = [0u8; 64];
    both[..32].copy_from_slice(&lang_start.0);
    both[32..].copy_from_slice(&main.0);
    Symbol(ContentId::of(SHIM_SYMBOL_DOMAIN, &both).0)
}

/// A code record of the run that is no item's instance (the module doc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Special<'r> {
    /// The C entry shim of a Rust program with std: `main(argc, argv)` calling
    /// `lang_start::<T>(main, argc, argv, SIGPIPE_DEFAULT)`.
    Shim(Shim<'r>),
    /// The thread-local getter (blaze-build `tls::index::getter`), called by
    /// [`tls_getter_symbol`] from every record that touches a thread-local.
    Getter,
    /// The thread-local ender (blaze-build `tls::index::ender`): the run's pthread key's
    /// destructor ([`tls_ender_symbol`]).
    Ender,
}

impl Special<'_> {
    /// How a walk's `Met` names it.
    pub const fn name(&self) -> &'static str {
        match self {
            Special::Shim(_) => SHIM_NAME,
            Special::Getter => GETTER_NAME,
            Special::Ender => ENDER_NAME,
        }
    }

    /// Its own parts of its key (the module doc).
    pub fn parts(&self) -> KeyParts {
        match self {
            Special::Shim(shim) => shim_parts(shim),
            Special::Getter => getter_parts(),
            Special::Ender => ender_parts(),
        }
    }

    /// Its entry, made for `isa` in `alloc`.
    fn make<'a>(&self, isa: Isa, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, CodeRecordError> {
        match self {
            Special::Shim(shim) => make_shim_entry(shim, isa, alloc),
            Special::Getter => make_getter_entry(isa, alloc),
            Special::Ender => make_ender_entry(isa, alloc),
        }
    }
}

/// Which special record `symbol` is, if any: the getter, the ender, or (a Rust start's) the
/// shim of `roots`. `None`: an instance's symbol, which the walk's table answers, or none the
/// run makes (the parent refuses it by name).
pub fn special_of<'r>(symbol: &Symbol, roots: &'r Roots<'_>) -> Option<Special<'r>> {
    if *symbol == tls_getter_symbol() {
        return Some(Special::Getter);
    }
    if *symbol == tls_ender_symbol() {
        return Some(Special::Ender);
    }
    match roots.shim() {
        Some(shim) if *symbol == roots.entry => Some(Special::Shim(shim)),
        _ => None,
    }
}

/// What [`make_special`] did: the entry's key in the store (its bytes are `store.get(key)`),
/// its parts, and whether it was found (else made and put now).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpecialMade {
    pub symbol: Symbol,
    pub key: CodeKey,
    pub parts: KeyParts,
    pub found: bool,
}

/// `which`'s entry for `builder`: found in `store` under its key, or made and put there as a
/// plain entry (made once: a repeat run finds it). `alloc` is the scratch for the entry until it
/// is put. The parent files and publishes the entry under `key`.
pub fn make_special(which: Special<'_>, builder: &Builder, store: &mut dyn CodeStore, alloc: Alloc<'_>) -> Result<SpecialMade, CodeRecordError> {
    let parts = which.parts();
    let key = code_key(&parts, builder);
    let found = store.get(key).map_err(CodeRecordError::Store)?.is_some();
    if !found {
        let entry = which.make(builder.isa, alloc)?;
        store.put(key, &entry).map_err(CodeRecordError::Store)?;
    }
    Ok(SpecialMade { symbol: parts.symbol, key, parts, found })
}

/// The shim's key parts: its symbol; a body digest of the generator's form ([`SHIM_DOMAIN`]),
/// `SIGPIPE_DEFAULT`, the two reference places, and each callee's tabled fn and symbol (which is
/// its name's hash); no ground facts.
fn shim_parts(shim: &Shim<'_>) -> KeyParts {
    let mut body = Hasher::new(SHIM_DOMAIN);
    body.part(&[blaze_build::start::SIGPIPE_DEFAULT]);
    for (place, row) in [(LANG_START_PLACE, shim.lang_start), (MAIN_PLACE, shim.main)] {
        body.part(&place.to_le_bytes());
        body.part(&row.tabled.0.to_le_bytes());
        body.part(&row.symbol.0);
    }
    let mut ground = Hasher::new(SHIM_DOMAIN);
    ground.part(b"no ground facts");
    KeyParts { symbol: shim_symbol(shim.lang_start.symbol, shim.main.symbol), body: body.finish(), ground: ground.finish() }
}

/// The shim's entry: blaze-build's `start::c_main_shim` built at [`PTR_BITS`], its two calls
/// named by the callees' symbols, declaring both callees (so the walk reaches them from it).
fn make_shim_entry<'a>(shim: &Shim<'_>, isa: Isa, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, CodeRecordError> {
    let mut b = blaze_lir::Builder::<Global, blaze_lir::Full>::new_in(PTR_BITS, Global);
    let lang_start = b.reference(RefKind::Func, LANG_START_PLACE);
    let main = b.reference(RefKind::Func, MAIN_PLACE);
    blaze_build::start::c_main_shim(&mut b, lang_start, main).map_err(CodeRecordError::Shim)?;
    let func = b.finish_checked().map_err(|error| CodeRecordError::Shim(blaze_build::BuildError::Lir(error)))?;
    // By place: `lang_start::<T>` at 0, `main` at 1.
    let funcs = [Callee::Symbol(shim.lang_start.symbol), Callee::Symbol(shim.main.symbol)];
    let names = InstanceNames { funcs: &funcs, statics: &[], vtables: &[], tls: &[] };
    let mut declared: AVec<'_, Declared<'_>> = Vec::new_in(alloc);
    declared.push(Declared { tabled: shim.lang_start.tabled, name: shim.lang_start.name });
    declared.push(Declared { tabled: shim.main.tabled, name: shim.main.name });
    make_entry(&func, &names, crate::front2::builtins_shim::Routines::NONE, &mut declared, isa, alloc)
}
