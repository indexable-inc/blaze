//! The run path's declarer (decision D106; blaze-tier record/declared.rs): what ONE built
//! instance names in the program, each with the data its record's key is made from without the
//! body, so the code record it emits declares every target and a walk follows it.
//!
//! The maker fills it from the instance's reference tables while it names them (the same places
//! `InstanceNames` names: `Built::refs`, `statics`, `vtables`, `thread_locals`, and the
//! thread-local runtime), then hands it to `blaze_tier::record::declare` before the seal. Each
//! row's symbol is computed HERE from its key data by symbols.rs, so a row and the target the
//! emitter made from the same reference cannot disagree (rule 76: one source for both).
//!
//! One table, in the caller's allocator, sorted by symbol when asked (rule 25); a repeated row
//! (the same callee named twice) is kept once by the seal.

use blaze_code::record::{Symbol, Target};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{Declare, DeclaredKind, Declares};
use blaze_typeck::ItemKey;

use crate::symbols::{fn_symbol_of_name, static_symbol, tls_desc_symbol, tls_getter_symbol, tls_symbol, vtable_symbol_of_parts};

/// One declared row before the seal: its symbol, kind, item and its name's range in the text.
#[derive(Clone, Copy, Debug)]
struct Row {
    symbol: Symbol,
    kind: DeclaredKind,
    item: u128,
    at: u32,
    len: u32,
}

/// Why a row was not added.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeclaredNamesError {
    /// The names' text would pass a `u32` (a code record's text is counted by one).
    TooLarge,
}

impl core::fmt::Display for DeclaredNamesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DeclaredNamesError::TooLarge => {
                write!(f, "one instance's declared names pass the 4 GiB a code record's text counts (blaze-ladder declared_names.rs, D106)")
            }
        }
    }
}

impl core::error::Error for DeclaredNamesError {}

/// What one built instance names, by symbol (the module doc).
pub struct DeclaredNames<'a> {
    rows: AVec<'a, Row>,
    text: AVec<'a, u8>,
    sorted: bool,
}

impl<'a> DeclaredNames<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> DeclaredNames<'a> {
        DeclaredNames { rows: Vec::new_in(alloc), text: Vec::new_in(alloc), sorted: true }
    }

    /// Empties the table for the next instance (a maker reuses one per instance).
    pub fn clear(&mut self) {
        self.rows.clear();
        self.text.clear();
        self.sorted = true;
    }

    fn push(&mut self, symbol: Symbol, kind: DeclaredKind, item: u128, name: &[u8]) -> Result<Symbol, DeclaredNamesError> {
        let at = self.text.len();
        let end = at.checked_add(name.len()).ok_or(DeclaredNamesError::TooLarge)?;
        let (Ok(at), Ok(len), Ok(_)) = (u32::try_from(at), u32::try_from(name.len()), u32::try_from(end)) else {
            return Err(DeclaredNamesError::TooLarge);
        };
        self.text.extend_from_slice(name);
        self.rows.push(Row { symbol, kind, item, at, len });
        self.sorted = false;
        Ok(symbol)
    }

    /// The fn instance named `name` (symbols.rs `encode_instance` bytes), built from the fn
    /// `tabled`; returns its symbol, the one its call sites use.
    pub fn instance(&mut self, tabled: ItemKey, name: &[u8]) -> Result<Symbol, DeclaredNamesError> {
        self.push(fn_symbol_of_name(name), DeclaredKind::Instance, tabled.0, name)
    }

    /// The `static` item `item`.
    pub fn static_item(&mut self, item: ItemKey) -> Result<Symbol, DeclaredNamesError> {
        self.push(static_symbol(item), DeclaredKind::Static, item.0, &[])
    }

    /// The vtable of `principal` whose concrete type's tree and trait argument trees are
    /// `trees` (symbols.rs `vtable_symbol`'s input after the principal).
    pub fn vtable(&mut self, principal: ItemKey, trees: &[u8], scratch: Alloc<'_>) -> Result<Symbol, DeclaredNamesError> {
        self.push(vtable_symbol_of_parts(principal, trees, scratch), DeclaredKind::Vtable, principal.0, trees)
    }

    /// The `#[thread_local]` static `item`.
    pub fn thread_local(&mut self, item: ItemKey) -> Result<Symbol, DeclaredNamesError> {
        self.push(tls_symbol(item), DeclaredKind::ThreadLocal, item.0, &[])
    }

    /// The program's thread-local descriptor and getter, which any record touching a
    /// thread-local names.
    pub fn tls_runtime(&mut self) -> Result<(), DeclaredNamesError> {
        self.push(tls_desc_symbol(), DeclaredKind::TlsDescriptor, 0, &[])?;
        self.push(tls_getter_symbol(), DeclaredKind::TlsGetter, 0, &[])?;
        Ok(())
    }

    /// Sorts the rows by symbol so [`Declares::declared`] halves; called once after the last push.
    pub fn finish(&mut self) {
        self.rows.sort_unstable_by(|a, b| a.symbol.cmp(&b.symbol));
        self.sorted = true;
    }
}

impl Declares for DeclaredNames<'_> {
    /// The row of `target`'s symbol whose kind is of its space; `None` (refused by the declare,
    /// by name) when the table was not finished or has none.
    fn declared(&self, target: Target) -> Option<Declare<'_>> {
        if !self.sorted {
            return None;
        }
        let first = self.rows.partition_point(|r| r.symbol < target.symbol);
        let row = self.rows[first..].iter().take_while(|r| r.symbol == target.symbol).find(|r| r.kind.space() == target.space)?;
        let name = self.text.get(row.at as usize..row.at as usize + row.len as usize)?;
        Some(Declare { kind: row.kind, item: row.item, name })
    }
}
