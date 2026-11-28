//! Same-program `extern` declarations bound to Rust `#[no_mangle]` / `#[export_name]`
//! definitions on the run path (as the native linker resolves them under AOT).
//!
//! When a Rust crate declares `extern "C" { fn foo(..); }` (or `#[link_name = "foo"]`) and a
//! linked crate of the same program defines `#[no_mangle] pub extern "C" fn foo(..)` or
//! `#[export_name = "foo"] pub extern "C" fn impl_foo(..)`, JIT execution cannot find `foo` via
//! `dlsym` because the Rust function is compiled on demand rather than linked into a host dylib.
//! Binding such declarations to their Rust definitions mirrors [`super::internal_symbol`] and
//! [`super::alloc_shim`]:
//! - When ONE strong Rust export in the program's link closure exports the declared symbol, the
//!   declaration's row is `FnKind::Free` with no foreign symbol and `resolve` answers the Rust
//!   definition (`List::EMPTY`), so both direct calls and function-pointer coercions target the
//!   JITted Rust function directly.
//! - When NO strong Rust export in the link closure matches (e.g. `libc`, `libSystem`, `libz`,
//!   C static archives), binding answers `Ok(None)` and the declaration stays `FnKind::Foreign`
//!   to be resolved via the native image / `dlsym` as before.
//! - When TWO OR MORE strong Rust exports in the link closure export the same symbol, binding
//!   refuses cleanly with [`ExportRefused::DefinedTwice`] naming the two smallest keys.
//! - Weak definitions (`#[linkage = "weak"]`, such as `compiler_builtins`'s fallback wrappers)
//!   are skipped so they yield to libc/libm and [`super::program::lazy::seam_rules::libm_float_intrinsic`],
//!   while `compiler_builtins`'s soft-float routines stay bound via [`super::builtins_shim`].

use core::fmt;

use blaze_id::ItemKey as LibKey;
use blaze_lower::head;
use blaze_own::{AString, AVec, Alloc};
use blaze_typeck::ItemKey;

use super::check::Records;
use super::heads::{CrateCfg, Store};
use super::keys::Keys;
use super::reach::Reached;
use super::source::NamesSource;

fn own_str<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    out.push_str(s);
    out
}

fn shown_lib<'n, 'a, S: NamesSource<'n>>(names: S, key: LibKey, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    let _ = fmt::write(&mut out, format_args!("{}", super::item_shown::ItemShown::of_lib(names, key)));
    out
}

/// Formats `why` into `alloc` without naming ambient `std::string::ToString`.
pub(crate) fn format_refused<'a>(why: &ExportRefused<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    let _ = fmt::write(&mut out, format_args!("{why}"));
    out
}

/// A foreign-block fn declaration eligible for same-program Rust export binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared<'a> {
    pub lib: LibKey,
    pub symbol: AString<'a>,
    pub params: usize,
    pub c_variadic: bool,
}

/// Why a same-program export binding failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportRefused<'a> {
    /// Two or more strong Rust definitions in the program's link closure export `symbol`:
    /// `first` and `second` are the two smallest by key (walk-order independent).
    DefinedTwice { symbol: AString<'a>, first: AString<'a>, second: AString<'a> },
    /// The declaration's or a candidate definition's attributes did not read.
    Attrs { symbol: AString<'a> },
    /// The definition's key collides with another item in the key map.
    KeyClash { symbol: AString<'a> },
    /// The declaration's place (and thus its block's ABI) is not in the names library.
    Unplaced { symbol: AString<'a> },
    /// The program's roots did not form a link closure (`NamesSource::linked`).
    Roots { symbol: AString<'a> },
    /// The names source could not supply one of the crates searched for the definition.
    Unread { symbol: AString<'a>, krate: usize },
}

impl ExportRefused<'_> {
    /// The refusal's class, for a survey's grouping.
    pub fn class(&self) -> &'static str {
        match self {
            ExportRefused::DefinedTwice { .. } => "an exported symbol defined twice in the program's linked crates",
            ExportRefused::Attrs { .. } => "a `#[no_mangle]`/`#[export_name]` or `#[link_name]` fn whose attributes do not read",
            ExportRefused::KeyClash { .. } => "the exported definition's key is another item's in the key map",
            ExportRefused::Unplaced { .. } => "a foreign declaration the names library does not place (its block's ABI is unknown)",
            ExportRefused::Roots { .. } => "the program's roots make no link closure for exported symbol lookup",
            ExportRefused::Unread { .. } => "the names source could not answer the exported symbol lookup",
        }
    }
}

impl fmt::Display for ExportRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportRefused::Attrs { symbol }
            | ExportRefused::KeyClash { symbol }
            | ExportRefused::Unplaced { symbol }
            | ExportRefused::Roots { symbol } => write!(f, "`{symbol}`: {}", self.class()),
            ExportRefused::DefinedTwice { symbol, first, second } => {
                write!(f, "`{symbol}`: {} (`{first}` and `{second}`)", self.class())
            }
            ExportRefused::Unread { symbol, krate } => write!(f, "`{symbol}` in crate {krate}: {}", self.class()),
        }
    }
}

/// The identifier a signature's lowering declares, `r#` dropped (Reference, `[abi.no_mangle]`).
fn declared_identifier<'b>(v: &blaze_lower::View<'b>) -> Option<&'b str> {
    let n = v.head_opt(head::NAME)?;
    let name = v.name_text(blaze_lower::NameIdx(n)).ok()?;
    Some(name.strip_prefix("r#").unwrap_or(name))
}

/// Reads the `#[link_name = "..."]` override on a declaration, when present, or detects
/// `#[rustc_intrinsic]` (which is handled by `intrinsic_rule`, never exported-symbol binding).
enum DeclAttrs<'a> {
    Intrinsic,
    LinkName(AString<'a>),
    Plain,
}

fn read_decl_attrs<'n, 'a, S: NamesSource<'n>>(
    names: S,
    krate: usize,
    values_at: LibKey,
    text: &str,
    alloc: Alloc<'a>,
) -> Result<DeclAttrs<'a>, ()> {
    if !text.contains("link_name") && !text.contains("rustc_intrinsic") {
        return Ok(DeclAttrs::Plain);
    }
    let atoms = names.cfg_atoms(krate).ok_or(())?;
    let edition = names.edition_of(krate).ok_or(())?;
    let extensions = names.blaze_extensions(krate);
    let sig = blaze_lower::lower_signature_with_extensions(text, edition, extensions, &mut CrateCfg::of(names, krate, atoms), alloc)
        .map_err(|_| ())?;
    let v = sig.view();
    let mut link_name: Option<AString<'a>> = None;
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| ())? {
        let a = v.attr(n).map_err(|_| ())?;
        match v.text_at(a.path).map_err(|_| ())?.trim() {
            "rustc_intrinsic" => return Ok(DeclAttrs::Intrinsic),
            "link_name" => {
                let args = v.text_at(a.args).map_err(|_| ())?;
                let sym = super::links::with_symbol_of(
                    args,
                    Some(values_at),
                    |item| names.attr_value(item, "link_name"),
                    edition,
                    |res| res.map(|s| own_str(s, alloc)).map_err(|_| ()),
                )?;
                link_name = Some(sym);
            }
            _ => {}
        }
    }
    Ok(match link_name {
        Some(sym) => DeclAttrs::LinkName(sym),
        None => DeclAttrs::Plain,
    })
}

/// Whether fn `key` is an `extern` declaration eligible for same-program Rust export binding,
/// returning its link symbol (`#[link_name]`, else its identifier with `r#` stripped).
///
/// Declarations handled by `llvm_intrinsic`, `panic_entry`, `alloc_shim`, `internal_symbol`, or
/// `#[rustc_intrinsic]` return `Ok(None)`.
pub fn declares<'n, 'a, S: NamesSource<'n>>(
    names: S,
    store: &Store<'n>,
    key: ItemKey,
    alloc: Alloc<'a>,
) -> Result<Option<Declared<'a>>, ExportRefused<'a>> {
    let Some(h) = store.fns.get(&key) else { return Ok(None) };
    if h.has_body || h.container.is_some() {
        return Ok(None);
    }
    let local_abi = super::panic_entry::local_abi(store, key);
    let raw_name = if local_abi.is_some() {
        super::panic_entry::local_name(store, key)
            .map_err(|_| ExportRefused::Attrs { symbol: shown_lib(names, h.lib, alloc) })?
    } else {
        names.name_of(h.lib)
    };
    let Some(raw_name) = raw_name else { return Ok(None) };
    let ident = raw_name.strip_prefix("r#").unwrap_or(raw_name);
    let abi = match local_abi {
        Some(local) => {
            if store.locals.foreign(key).is_some_and(|f| f.wasm_import) {
                return Ok(None);
            }
            Some(local)
        }
        None => {
            if names.is_wasm_import(h.lib).unwrap_or(false) {
                return Ok(None);
            }
            names.extern_abi(h.lib).ok_or_else(|| ExportRefused::Unplaced { symbol: own_str(ident, alloc) })?
        }
    };
    let Some(abi) = abi else { return Ok(None) };
    if abi == super::llvm_intrinsic::ABI {
        return Ok(None);
    }
    // Defer to dedicated binding seams that own these declarations.
    if super::panic_entry::declares(names, store, key, alloc).unwrap_or(false) {
        return Ok(None);
    }
    if super::alloc_shim::declares(names, store, key, alloc).is_ok_and(|s| s.is_some()) {
        return Ok(None);
    }
    if super::internal_symbol::declares(names, store, key, alloc).is_ok_and(|d| d.is_some()) {
        return Ok(None);
    }
    let attrs_err = || ExportRefused::Attrs { symbol: own_str(ident, alloc) };
    let krate = names.crate_of(h.lib).ok_or_else(attrs_err)?;
    let values_at = super::links::values_key(names, store, h.lib);
    let params = h.inputs.len();
    let c_variadic = h.c_variadic;
    match read_decl_attrs(names, krate, values_at, h.text, alloc).map_err(|()| attrs_err())? {
        DeclAttrs::Intrinsic => Ok(None),
        DeclAttrs::LinkName(symbol) => Ok(Some(Declared { lib: h.lib, symbol, params, c_variadic })),
        DeclAttrs::Plain => Ok(Some(Declared { lib: h.lib, symbol: own_str(ident, alloc), params, c_variadic })),
    }
}

/// Checks whether candidate function `key` in `krate` strongly exports `symbol` (i.e. has a body,
/// is not cfg'd out, is not `#[linkage = "weak"]`, matches `sig_shape` when given, and its
/// effective exported symbol — with `#[export_name]` taking precedence over `#[no_mangle]` per
/// Reference `[abi.export_name]` — equals `symbol`).
fn exports_symbol<'n, 'a, S: NamesSource<'n>>(
    names: S,
    krate: usize,
    key: LibKey,
    symbol: &str,
    sig_shape: Option<(usize, bool)>,
    alloc: Alloc<'a>,
) -> Result<bool, ExportRefused<'a>> {
    let unread = || ExportRefused::Unread { symbol: own_str(symbol, alloc), krate };
    let attrs_err = || ExportRefused::Attrs { symbol: own_str(symbol, alloc) };
    let text = names.text_of(key).ok_or_else(unread)?.text;
    let atoms = names.cfg_atoms(krate).ok_or_else(unread)?;
    let edition = names.edition_of(krate).ok_or_else(unread)?;
    let extensions = names.blaze_extensions(krate);
    let sig = blaze_lower::lower_signature_with_extensions(text, edition, extensions, &mut CrateCfg::of(names, krate, atoms), alloc)
        .map_err(|_| attrs_err())?;
    let v = sig.view();
    if v.cfg_out() {
        return Ok(false);
    }
    let flags = v.head.get(head::FLAGS).copied().unwrap_or(0);
    if flags & blaze_lower::machine::sig::HAS_BODY == 0 {
        return Ok(false);
    }
    if let Some((want_params, want_variadic)) = sig_shape {
        let def_variadic = flags & blaze_lower::machine::sig::VARIADIC != 0;
        if def_variadic != want_variadic {
            return Ok(false);
        }
        let mut def_params = 0usize;
        for &n in v.rows(v.list(head::PARAMS)).map_err(|_| attrs_err())? {
            let prm = blaze_lower::Param::decode(n).map_err(|_| attrs_err())?;
            if !prm.variadic {
                def_params = def_params.saturating_add(1);
            }
        }
        if def_params != want_params {
            return Ok(false);
        }
    }
    let mut no_mangle = false;
    let mut export_name: Option<AString<'a>> = None;
    let mut weak = false;
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| attrs_err())? {
        let a = v.attr(n).map_err(|_| attrs_err())?;
        match v.text_at(a.path).map_err(|_| attrs_err())?.trim() {
            "no_mangle" => no_mangle = true,
            "export_name" => {
                let args = v.text_at(a.args).map_err(|_| attrs_err())?;
                let sym = super::links::with_symbol_of(
                    args,
                    Some(key),
                    |item| names.attr_value(item, "export_name"),
                    edition,
                    |res| res.map(|s| own_str(s, alloc)).map_err(|_| attrs_err()),
                )?;
                export_name = Some(sym);
            }
            "linkage" => {
                let args = v.text_at(a.args).map_err(|_| attrs_err())?;
                if args.trim().trim_start_matches('=').trim().trim_matches('"') == "weak" {
                    weak = true;
                }
            }
            _ => {}
        }
    }
    if weak {
        return Ok(false);
    }
    // Reference [abi.export_name]: when both `no_mangle` and `export_name` are applied,
    // `export_name` wins.
    if let Some(ref exported) = export_name {
        return Ok(exported.as_str() == symbol);
    }
    if no_mangle {
        return Ok(declared_identifier(&v) == Some(symbol));
    }
    Ok(false)
}

/// Computes the sorted, deduplicated non-proc-macro linked crate indices for `roots`.
pub fn linked_crates<'n, 'a, S: NamesSource<'n>>(
    names: S,
    roots: &[usize],
    symbol: &str,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, usize>, ExportRefused<'a>> {
    let linked = match names.linked(roots, alloc) {
        Some(Ok(linked)) => linked,
        Some(Err(_)) => return Err(ExportRefused::Roots { symbol: own_str(symbol, alloc) }),
        None => {
            return Err(ExportRefused::Unread {
                symbol: own_str(symbol, alloc),
                krate: roots.first().copied().unwrap_or(0),
            });
        }
    };
    let mut out = AVec::new_in(alloc);
    for krate in linked.crates() {
        let unread = || ExportRefused::Unread { symbol: own_str(symbol, alloc), krate };
        if names.proc_macro(krate).ok_or_else(unread)? {
            continue;
        }
        out.push(krate);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Searches `crates` (skipping any crate in `skip`) for a strong Rust `#[no_mangle]` /
/// `#[export_name]` function exporting `symbol`.
fn definition_in_tier<'n, 'a, S: NamesSource<'n>>(
    names: S,
    crates: &[usize],
    skip: &[usize],
    symbol: &str,
    sig_shape: Option<(usize, bool)>,
    alloc: Alloc<'a>,
) -> Result<Option<LibKey>, ExportRefused<'a>> {
    let mut found: Option<LibKey> = None;
    let mut second: Option<LibKey> = None;
    for &krate in crates {
        if skip.contains(&krate) {
            continue;
        }
        let unread = || ExportRefused::Unread { symbol: own_str(symbol, alloc), krate };
        if names.proc_macro(krate).ok_or_else(unread)? {
            continue;
        }
        let mut candidates: AVec<'_, LibKey> = AVec::new_in(alloc);
        let listed = names.each_export_candidate(krate, alloc, &mut |key, name, has_no_mangle, has_export_name| {
            // Fast pre-filter: a `#[no_mangle]`-only item can only export its own identifier, so
            // skip it without opening its source file unless its name matches `symbol`.
            if !has_export_name {
                if !has_no_mangle {
                    return;
                }
                let ident = name.map(|n| n.strip_prefix("r#").unwrap_or(n));
                if ident != Some(symbol) {
                    return;
                }
            }
            candidates.push(key);
        });
        if !listed {
            return Err(unread());
        }
        for &key in candidates.iter() {
            if !exports_symbol(names, krate, key, symbol, sig_shape, alloc)? {
                continue;
            }
            // Keep the two smallest keys: the refusal names the same pair in any walk order.
            match found {
                None => found = Some(key),
                Some(f) if key == f => {}
                Some(f) if key < f => {
                    second = Some(f);
                    found = Some(key);
                }
                Some(_) => {
                    if second.is_none_or(|s| key < s) {
                        second = Some(key);
                    }
                }
            }
        }
    }
    if let (Some(first), Some(second)) = (found, second) {
        return Err(ExportRefused::DefinedTwice {
            symbol: own_str(symbol, alloc),
            first: shown_lib(names, first, alloc),
            second: shown_lib(names, second, alloc),
        });
    }
    Ok(found)
}

/// Searches `crates` (already filtered to non-proc-macro crates in the program's link closure)
/// for a strong Rust `#[no_mangle]` / `#[export_name]` function exporting `symbol`.
///
/// When `override_crates` is non-empty (explicitly injected host runtime shim crates such as
/// `blaze_wasm64_pm_shim` for self-compiled wasm64 proc-macros), strong definitions in
/// `override_crates` take precedence over non-override crates. Duplicate strong definitions
/// *within* `override_crates` or *within* non-override `crates` are still refused with
/// [`ExportRefused::DefinedTwice`].
///
/// Returns `Ok(None)` when no searched crate exports `symbol` (allowing the caller to fall
/// through to `FnKind::Foreign` / `dlsym`), `Ok(Some(key))` when uniquely defined, and
/// `Err(ExportRefused::DefinedTwice { .. })` when two or more definitions match in the winning
/// tier.
pub fn definition_in<'n, 'a, S: NamesSource<'n>>(
    names: S,
    crates: &[usize],
    override_crates: &[usize],
    symbol: &str,
    sig_shape: Option<(usize, bool)>,
    alloc: Alloc<'a>,
) -> Result<Option<LibKey>, ExportRefused<'a>> {
    if !override_crates.is_empty()
        && let Some(found) = definition_in_tier(names, override_crates, &[], symbol, sig_shape, alloc)?
    {
        return Ok(Some(found));
    }
    definition_in_tier(names, crates, override_crates, symbol, sig_shape, alloc)
}

/// The strong Rust definition exporting `symbol` in the link closure of `roots`, if any.
pub fn definition<'n, 'a, S: NamesSource<'n>>(
    names: S,
    roots: &[usize],
    symbol: &str,
    sig_shape: Option<(usize, bool)>,
    alloc: Alloc<'a>,
) -> Result<Option<LibKey>, ExportRefused<'a>> {
    let crates = linked_crates(names, roots, symbol, alloc)?;
    definition_in(names, crates.as_slice(), &[], symbol, sig_shape, alloc)
}

/// Resolves a declared foreign fn to its same-program Rust export's checker key, if any.
pub fn bound<'n, 'a, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    roots: &[usize],
    d: &Declared<'_>,
    alloc: Alloc<'a>,
) -> Result<Option<ItemKey>, ExportRefused<'a>> {
    let Some(lib) = definition(names, roots, d.symbol.as_str(), Some((d.params, d.c_variadic)), alloc)? else {
        return Ok(None);
    };
    let key = keys.item(lib).map_err(|_| ExportRefused::KeyClash { symbol: own_str(d.symbol.as_str(), alloc) })?;
    Ok(Some(key))
}

/// Resolves a declared foreign fn using precomputed `crates` and optional `override_crates`,
/// returning its checker key if any.
pub fn bound_in<'n, 'a, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    crates: &[usize],
    override_crates: &[usize],
    d: &Declared<'_>,
    alloc: Alloc<'a>,
) -> Result<Option<ItemKey>, ExportRefused<'a>> {
    let Some(lib) = definition_in(names, crates, override_crates, d.symbol.as_str(), Some((d.params, d.c_variadic)), alloc)? else {
        return Ok(None);
    };
    let key = keys.item(lib).map_err(|_| ExportRefused::KeyClash { symbol: own_str(d.symbol.as_str(), alloc) })?;
    Ok(Some(key))
}

/// One covering read for all exported-function facts of this crate (`NameCell::Exports`), absent
/// for proc-macro crates and crates with no exported functions so ordinary edits wake no callers.
pub(crate) fn search_read(key: blaze_id::CrateItemKey) -> super::tables::recorder::TableRead {
    use blaze_scope::names_cells::NameCell;
    super::tables::recorder::TableRead::Names(NameCell::Exports { krate: key.prefix() })
}

/// Records `NameCell::Exports` for every crate in scope into the caller's `BodyCheck` reads
/// whenever a checked body referenced any foreign fn head eligible for export binding (rule 21).
/// Covers both positive hits and negative misses so adding, removing, or editing a `#[no_mangle]`
/// or `#[export_name]` fn invalidates dependent `BodyCheck` and code keys.
pub(crate) fn record_search<'n>(
    tables: super::tables::view::Tables<'_, 'n>,
    store: &Store<'n>,
    facts: &super::reads::BodyReads<'_>,
) {
    use super::reads::FactKind;
    let searches = facts.rows.iter().filter(|r| r.fact.kind == FactKind::Head).any(|r| {
        !matches!(declares(tables, store, ItemKey(r.fact.key), store.work_alloc()), Ok(None))
    });
    if searches {
        for k in 0..tables.crate_count() {
            let Some(key) = tables.crate_key(k) else {
                tables.gap("a crate in the exported symbol search has no crate key", None);
                return;
            };
            tables.reads().read(search_read(key));
        }
    }
}

fn failed(why: &ExportRefused<'_>, alloc: Alloc<'_>) -> Reached {
    Reached::Failed {
        class: why.class().into(),
        detail: format_refused(why, alloc).as_str().into(),
        refusal: None,
    }
}

/// The eager reach walk's outcome for `k`, a fn whose head says it has no body, when it is an
/// `extern` declaration bound to a same-program Rust export: `Some((Reached::NoBody, Some(def)))`
/// (queueing `def` as its callee), `Some((Reached::Failed { .. }, None))` on refusal, or `None`
/// when `k` is not bound to a same-program Rust export (staying an external C symbol).
pub fn reached(r: &mut Records<'_, '_>, k: ItemKey, alloc: Alloc<'_>) -> Option<(Reached, Option<ItemKey>)> {
    let d = match declares(r.names, r.store, k, alloc) {
        Ok(None) => return None,
        Err(why) => return Some((failed(&why, alloc), None)),
        Ok(Some(d)) => d,
    };
    match bound(r.names, r.keys, r.index.scope(), &d, alloc) {
        Ok(None) => None,
        Ok(Some(def)) => Some((Reached::NoBody, Some(def))),
        Err(why) => Some((failed(&why, alloc), None)),
    }
}
