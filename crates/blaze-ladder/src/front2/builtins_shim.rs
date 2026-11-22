//! The soft-float routines on the run path (port27/tempo-run/INTRINSICS.md 3.5, P5b): every
//! `f128` operation (and an `f16` conversion through `f128`) is a call of the program's own
//! compiler_builtins routine for it (blaze-lir legalize, `SoftFloat`), bound by symbol from
//! that crate's `#[no_mangle]` exports, as [`super::alloc_shim`] binds `__rust_alloc` to std's
//! default. Never from libSystem: the program's compiler_builtins is the one rustc links too,
//! and a routine it lacks is a typed refusal naming it ([`BuiltinsRefused`]).
//!
//! WHICH routines a function calls is an output of emitting it (main's decision, 2026-11-16):
//! only legalize maps an operation to its routine, so nothing here predicts it.
//! - [`find`] (asked once a run, through the seam's `FrontEnd::soft_float_routines`) finds each
//!   routine of `SoftFloat::ALL` (the tf routines and libm's `fmodf128`, which compiler_builtins
//!   exports too: src/math/mod.rs `full_availability`) among the fns of the compiler_builtins
//!   the program links, by its exported name;
//! - [`Bound`] holds each found routine's instance name and symbol (symbols.rs, a fn with no
//!   generic arguments), made once a run by the record walk's maker;
//! - [`Bound::bind`] appends one callee per found routine to an instance's function
//!   references and gives the emitter's table ([`Routines`], blaze-emit `RoutinePlaces`);
//! - code_records.rs `make_entry` declares exactly the routines the emitter answers it called
//!   ([`Routines::declared`]): bound at first call like any other reference, listed in the
//!   record's declared section like any other instance.
//!
//! The pinned compiler_builtins (library/compiler-builtins/compiler-builtins/src/macros.rs, the
//! final catch-all arm of `intrinsics!`) writes each routine twice: a mangled
//! `pub extern "C" fn __addtf3` and, under `#[cfg(feature = "unmangled-names")]`,
//! `mod __addtf3 { #[unsafe(no_mangle)] extern "C" fn __addtf3(..) { super::__addtf3(..) } }`.
//! The export is the second: so a routine is the fn of compiler_builtins named its symbol
//! whose own attributes hold `no_mangle` (Reference, abi.no_mangle: "the symbol for the item
//! is the item's identifier"), never the fn of that name alone.

use core::fmt;

use blaze_code::record::{RecordError, Symbol};
use blaze_emit::{RecordEmitError, RoutinePlaces};
use blaze_lir::legalize::{LegalizeError, SoftFloat};
use blaze_lower::head;
use blaze_own::{AVec, Alloc};
use blaze_typeck::{ItemKey, List, PortableTys};

use super::heads::CrateCfg;
use super::keys::checker_of;
use super::source::NamesSource;
use crate::code_records::{Callee, CodeRecordError, Declared};
use crate::symbols::{SymbolError, encode_instance, fn_symbol_of_name};

/// The crate whose exports the routines are.
pub const CRATE: &str = "compiler_builtins";

const COUNT: usize = SoftFloat::ALL.len();

/// What [`find`] found for one routine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    /// The exported fn, by its checker key.
    Key(ItemKey),
    /// compiler_builtins has no fn of this name with `no_mangle` (its cfg left it out, or the
    /// toolchain's compiler_builtins does not define it).
    Missing,
    /// Two fns of compiler_builtins export this name.
    Twice,
    /// The program (the link closure of its roots, linked.rs) links no crate named
    /// `compiler_builtins`.
    NoCrate,
    /// The program links two crates named `compiler_builtins`. Two in the GRAPH are not this:
    /// a workspace's graph holds every program's crates (and a unit of another side or
    /// feature set); only the ones the program links are its (the rule its lang rows follow,
    /// index.rs `Index::new`).
    CrateTwice,
    /// The program's roots did not make a link closure (a root of no crate: linked.rs
    /// `RootsError`).
    Roots,
    /// A fn named the routine's symbol whose signature's attributes do not read.
    Attrs,
    /// The names source did not answer (a crate's kind, its items, cfg or edition): the error
    /// is in the source's log.
    Unanswered,
    /// The front end does not look routines up (the old front): both front2 programs do.
    NotLookedUp,
}

/// Each routine's [`Found`], indexed by `SoftFloat as usize`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Builtins {
    found: [Found; COUNT],
    /// Each [`Found::Key`]'s library key: what the eager program reaches the routine from (its
    /// row is made before the build, program.rs `make_from`).
    libs: [Option<blaze_id::ItemKey>; COUNT],
}

impl Builtins {
    /// The answer of a front end that does not look routines up.
    pub const NOT_LOOKED_UP: Builtins = Builtins { found: [Found::NotLookedUp; COUNT], libs: [None; COUNT] };

    pub fn found(&self, r: SoftFloat) -> Found {
        self.found[r as usize]
    }

    /// Routine `r`'s exported fn by its library key, when [`Self::found`] is a key.
    pub fn lib(&self, r: SoftFloat) -> Option<blaze_id::ItemKey> {
        match self.found[r as usize] {
            Found::Key(_) => self.libs[r as usize],
            _ => None,
        }
    }

    /// Each routine's exported fn given (a test's stand-in for [`find`]).
    #[cfg(test)]
    pub(crate) fn of_keys(keys: [ItemKey; COUNT]) -> Builtins {
        Builtins { found: keys.map(Found::Key), libs: [None; COUNT] }
    }
}

/// A routine an `f128` operation needs that the program's compiler_builtins does not give.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BuiltinsRefused {
    pub routine: SoftFloat,
    pub why: Found,
}

impl fmt::Display for BuiltinsRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let symbol = self.routine.symbol();
        let why = match self.why {
            Found::Key(_) => "found, yet not bound (a bug of front2/builtins_shim.rs)",
            Found::Missing => "compiler_builtins exports no fn of this name (`#[no_mangle]`): its cfg left it out, or the toolchain's compiler_builtins lacks it",
            Found::Twice => "compiler_builtins exports two fns of this name",
            Found::NoCrate => "the program links no crate named `compiler_builtins`",
            Found::CrateTwice => "the program links two crates named `compiler_builtins`",
            Found::Roots => "the program's roots make no link closure (a root of no crate)",
            Found::Attrs => "a compiler_builtins fn of this name whose attributes do not read",
            Found::Unanswered => "the names records did not answer about compiler_builtins (the error is in the source's log)",
            Found::NotLookedUp => "this front end does not look compiler_builtins' routines up (the record walk over front2 does)",
        };
        write!(f, "an `f128` operation needs the soft-float routine `{symbol}` of the program's compiler_builtins (INTRINSICS.md 3.5; never libSystem's): {why}")
    }
}

impl core::error::Error for BuiltinsRefused {}

/// Each routine's exported fn in the compiler_builtins the program of `roots` links (the link
/// closure of `roots`, linked.rs: the one reading of the link rule, which the program's lang
/// rows follow too; callers pass the roots their index was filed for, index.rs
/// `Index::scope`). A crate of the graph the program does not link is never looked at: the
/// census of tempo (port27/tempo-run/build-census-g002415.txt) refused 41 fns with
/// "two crates of the graph are named `compiler_builtins`" when the whole graph was read.
/// Reads that one crate's item rows once (`each_item`: no text), then lowers the signature of
/// only the fns named one of the routines' symbols (two each in the pinned crate) to read their
/// `no_mangle`. `alloc` holds the closure, the candidates and the lowerings, dropped on return.
pub fn find<'n, S: NamesSource<'n>>(names: S, roots: &[usize], alloc: Alloc<'_>) -> Builtins {
    let all = |found: Found| Builtins { found: [found; COUNT], libs: [None; COUNT] };
    let linked = match names.linked(roots, alloc) {
        Some(Ok(linked)) => linked,
        Some(Err(_)) => return all(Found::Roots),
        None => return all(Found::Unanswered),
    };
    let mut krate = None;
    for k in linked.crates() {
        match names.proc_macro(k) {
            Some(true) => continue,
            Some(false) => {}
            None => return all(Found::Unanswered),
        }
        if names.root_module(k).and_then(|m| names.module_path(m)) != Some(CRATE) {
            continue;
        }
        if krate.replace(k).is_some() {
            return all(Found::CrateTwice);
        }
    }
    let Some(krate) = krate else { return all(Found::NoCrate) };
    let (Some(atoms), Some(edition)) = (names.cfg_atoms(krate), names.edition_of(krate)) else {
        return all(Found::Unanswered);
    };
    // Every fn of the crate named a routine's symbol: (routine, key).
    let mut named: AVec<'_, (SoftFloat, blaze_id::ItemKey)> = AVec::new_in(alloc);
    let read = names.each_item(krate, &mut |key, _| {
        if names.kind_of_item(key) != Some(blaze_surface::ItemKind::Function) {
            return;
        }
        let Some(name) = names.name_of(key) else { return };
        if let Some(r) = SoftFloat::ALL.into_iter().find(|r| r.symbol() == name) {
            named.push((r, key));
        }
    });
    if !read {
        return all(Found::Unanswered);
    }
    let mut out = all(Found::Missing);
    for &(r, key) in named.iter() {
        let at = r as usize;
        let exported = match no_mangle(names, key, atoms, edition, alloc) {
            Ok(exported) => exported,
            Err(why) => {
                out.found[at] = why;
                continue;
            }
        };
        if !exported {
            continue;
        }
        let k = checker_of(key);
        out.libs[at] = Some(key);
        out.found[at] = match out.found[at] {
            Found::Missing => Found::Key(k),
            Found::Key(have) if have == k => Found::Key(k),
            Found::Key(_) | Found::Twice => Found::Twice,
            other => other,
        };
    }
    out
}

/// Whether fn `key`'s own attributes hold `no_mangle` (`#[unsafe(no_mangle)]`'s row has the
/// inner path: blaze-lower's UNSAFE_META, as front2/links.rs reads it).
fn no_mangle<'n, S: NamesSource<'n>>(
    names: S,
    key: blaze_id::ItemKey,
    atoms: &'n [(&'n str, Option<&'n str>)],
    edition: blaze_surface::Edition,
    alloc: Alloc<'_>,
) -> Result<bool, Found> {
    let text = names.text_of(key).ok_or(Found::Unanswered)?.text;
    if !text.contains("no_mangle") {
        return Ok(false);
    }
    let sig = blaze_lower::lower_signature(text, edition, &mut CrateCfg(atoms), alloc).map_err(|_| Found::Attrs)?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| Found::Attrs)? {
        let a = v.attr(n).map_err(|_| Found::Attrs)?;
        if v.text_at(a.path).map_err(|_| Found::Attrs)?.trim() == "no_mangle" {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The found routines as instances, once a run: each one's tabled fn, instance name (in
/// `text`, at `spans`) and symbol.
pub struct Bound {
    found: Builtins,
    symbols: [Option<Symbol>; COUNT],
    spans: [(u32, u32); COUNT],
    text: AVec<'static, u8>,
}

impl Bound {
    /// Each found routine's instance: its fn with no generic arguments (`List::EMPTY`), named
    /// by symbols.rs `encode_instance` as every declared instance is.
    pub fn new(found: Builtins, ground: &PortableTys<'_>, alloc: Alloc<'static>) -> Result<Bound, SymbolError> {
        let mut bound = Bound { found, symbols: [None; COUNT], spans: [(0, 0); COUNT], text: AVec::new_in(alloc) };
        for r in SoftFloat::ALL {
            let Found::Key(item) = found.found(r) else { continue };
            let at = bound.text.len();
            encode_instance(item, List::EMPTY, ground.view(), alloc, &mut bound.text)?;
            let (Ok(start), Ok(len)) = (u32::try_from(at), u32::try_from(bound.text.len() - at)) else {
                return Err(SymbolError::NotGround { what: "a soft-float routine's instance name past 4 GiB" });
            };
            bound.spans[r as usize] = (start, len);
            bound.symbols[r as usize] = Some(fn_symbol_of_name(&bound.text[at..]));
        }
        Ok(bound)
    }

    /// Appends one callee per found routine to an instance's function references `funcs` (in
    /// [`SoftFloat::ALL`]'s order, after the instance's own) and gives the emitter's table of
    /// their places.
    pub fn bind<'b>(&'b self, funcs: &mut AVec<'_, Callee<'_>>) -> Result<Routines<'b>, CodeRecordError> {
        let mut places = RoutinePlaces::NONE;
        for r in SoftFloat::ALL {
            let Some(symbol) = self.symbols[r as usize] else { continue };
            let place = u32::try_from(funcs.len())
                .ok()
                .filter(|&p| p != u32::MAX)
                .ok_or(CodeRecordError::Seal(RecordError::TooLarge { what: "an instance's function references" }))?;
            places.bind(r, place);
            funcs.push(Callee::Symbol(symbol));
        }
        Ok(Routines { places, bound: Some(self) })
    }

    fn declared(&self, r: SoftFloat) -> Option<Declared<'_>> {
        let Found::Key(tabled) = self.found.found(r) else { return None };
        self.symbols[r as usize]?;
        let (start, len) = self.spans[r as usize];
        Some(Declared { tabled, name: &self.text[start as usize..][..len as usize] })
    }
}

/// The emitter's routine table for one instance, and how a called routine is declared.
#[derive(Clone, Copy)]
pub struct Routines<'r> {
    places: RoutinePlaces,
    bound: Option<&'r Bound>,
}

impl<'r> Routines<'r> {
    /// No routine bound (the thread-local getter and ender): an `f128`
    /// operation is refused naming its routine.
    pub const NONE: Routines<'static> = Routines { places: RoutinePlaces::NONE, bound: None };

    pub fn places(&self) -> &RoutinePlaces {
        &self.places
    }

    /// Routine `r`, which the emitter answered the record calls, as the instance it declares.
    pub fn declared(&self, r: SoftFloat) -> Result<Declared<'r>, CodeRecordError> {
        let why = |why| CodeRecordError::Routine(BuiltinsRefused { routine: r, why });
        let bound = self.bound.ok_or(why(Found::NotLookedUp))?;
        bound.declared(r).ok_or(why(bound.found.found(r)))
    }

    /// The emitter's refusal as the record's: a routine not bound is named with why
    /// compiler_builtins did not give it.
    pub fn refusal(&self, error: RecordEmitError) -> CodeRecordError {
        match (error, self.bound) {
            (RecordEmitError::Legalize(LegalizeError::NoRoutine(r)), Some(bound)) => {
                CodeRecordError::Routine(BuiltinsRefused { routine: r, why: bound.found.found(r) })
            }
            (error, _) => CodeRecordError::Emit(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_emit::{Isa, Os};
    use blaze_lir::builder::{Param, Variadic};
    use blaze_lir::func::Conv;
    use blaze_lir::legalize::SoftFloat;
    use blaze_lir::{BlockKind, Builder, Flags, Full, Func, Op, Ty};
    use blaze_own::{AVec, Alloc};
    use blaze_typeck::{ItemKey, PortableTys};

    use super::{Bound, Builtins, COUNT};
    use crate::code_records::{Callee, Declared, EntryView, InstanceNames, make_entry};

    /// `fn(a: f128, b: f128) -> f128 { a + b }`, calling nothing of its own.
    fn add_f128() -> Func<Global> {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let q = Param::of(Ty::F128);
        let sig = b.sig(Conv::C, Variadic::No, &[q, q], &[q]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let (x, y) = (b.param(Ty::F128), b.param(Ty::F128));
        let sum = b.binary(Op::Fadd, Flags::NONE, x, y);
        b.ret(&[sum]);
        b.finish_checked().expect("the f128 add is valid LIR")
    }

    /// INTRINSICS.md 3.5 and main's P5b decision: which routines a function calls is an
    /// output of emitting it, and the run path declares exactly those. With all 24 routines of
    /// compiler_builtins bound, the entry of a function holding one `f128` add declares
    /// `__addtf3`'s instance and nothing else of the 24. A `make_entry` that ignored the
    /// emitter's list of routines called would declare none (the control: this test fails).
    ///
    /// Prints: `test front2::builtins_shim::tests::one_f128_add_declares_addtf3_only ... ok`
    /// (once un-ignored).
    #[test]
    fn one_f128_add_declares_addtf3_only() {
        let heap = blaze_own::testing::heap();
        let alloc: Alloc<'static> = heap.alloc();
        let keys: [ItemKey; COUNT] = core::array::from_fn(|k| ItemKey(0xb0_0000 + k as u128));
        let ground = PortableTys::new_in(alloc);
        let bound = Bound::new(Builtins::of_keys(keys), &ground, alloc).expect("24 plain fns name their instances");
        let mut funcs: AVec<'_, Callee<'_>> = AVec::new_in(alloc);
        let routines = bound.bind(&mut funcs).expect("24 places fit");
        assert_eq!(funcs.len(), COUNT, "one function reference per routine");
        let names = InstanceNames { funcs: &funcs, statics: &[], c_statics: &[], vtables: &[], tls: &[] };
        let mut declared: AVec<'_, Declared<'_>> = AVec::new_in(alloc);
        let isa = Isa { lse: true, os: Os::MacOs };
        let func = add_f128();
        let entry = make_entry(blaze_lir::verify(func.view()).expect("the f128 add is valid LIR"), &names, routines, &mut declared, isa, alloc)
            .unwrap_or_else(|why| panic!("an f128 add with every routine bound makes an entry: {why}"));
        let view = EntryView::read(&entry).unwrap_or_else(|why| panic!("the entry reads: {why}"));
        let mut tabled: std::vec::Vec<ItemKey> = std::vec::Vec::new();
        view.each_declared(|d| -> Result<(), ()> { tabled.push(d.tabled); Ok(()) }, |_| ()).expect("the declared section reads");
        let add = keys[SoftFloat::AddTf3 as usize];
        assert_eq!(tabled, [add], "declared {tabled:x?}; want only __addtf3's fn {:#x}", add.0);
        let want = bound.declared(SoftFloat::AddTf3).expect("__addtf3 is bound");
        assert_eq!(declared.as_slice(), &[want], "the caller's declared list holds __addtf3's instance alone");
    }
}
