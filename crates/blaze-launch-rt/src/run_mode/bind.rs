//! THE BINDER SEAM (D175, comptime-native.md "The table bound"; rule 78): how the run gives a C
//! name a record calls or reads its address. One seam, run_mode.rs `Binder::outside`, and
//! three answers, in this order:
//!
//! 1. A C name the PROGRAM defines (a C unit's global atom, blaze-cunit; a Rust
//!    `#[no_mangle]` fn or static) is a symbol the parent declared (`Down::Declare`, a function
//!    slot) or told (`Down::Data`, a static): its symbol is the name's own
//!    (`Symbol::of_name`), so a record's `Space::Outside` target of it resolves to that slot
//!    or static BEFORE any strategy is asked (run_mode.rs `Bound`). No flag on the wire: the
//!    symbol's domain is the statement (rule 30).
//! 2. Otherwise the run's strategy [`Bind`], a type parameter of the driver's memory:
//!    [`Dlsym`] for an ordinary run (the process's own images), [`Table`] for a compile-time
//!    run (blaze-comptime-sys's table: the layer's function, or the process's own definition
//!    for a row the table answers `Own`; a name not in it is refused at bind time, never bound
//!    to the real C library).
//! 3. A refusal is [`super::RunModeError::Outside`] naming the record, the symbol and the name.
//!
//! The two strategies are tested equal where they must be (run_mode/bind_tests.rs): a name the
//! table answers `Own` binds to the same address under both.

use core::ffi::CStr;

use blaze_comptime_sys::child::{Binding, binding};
use blaze_comptime_sys::{How, NAMES, Os, Unbound, find};

/// How a run binds a C name no record of the program defines.
pub trait Bind {
    /// The address C name `name` binds to (`cname`: the same bytes, NUL-ended), or why it does
    /// not, in words [`super::RunModeError::Outside`] carries.
    fn address(&mut self, name: &str, cname: &CStr) -> Result<u64, &'static str>;
}

/// An ordinary run's binding: the images this process loaded, in the loader's order
/// (`dlsym(RTLD_DEFAULT, name)`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Dlsym;

impl Bind for Dlsym {
    fn address(&mut self, _: &str, cname: &CStr) -> Result<u64, &'static str> {
        #[cfg(target_os = "macos")]
        // SAFETY: RTLD_DEFAULT searches all loaded images and cname is NUL-terminated.
        let address = unsafe {
            blaze_sys::posix::thread::dlsym(blaze_sys::posix::thread::RTLD_DEFAULT, cname.as_ptr())
        };
        #[cfg(not(target_os = "macos"))]
        // SAFETY: a lookup, by a live NUL-terminated name, among the images this process
        // loaded; it reads the name and returns an address or null.
        let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) };
        match address.is_null() {
            true => Err("no image of this process defines it (`dlsym`)"),
            false => Ok(address as u64),
        }
    }
}

/// The most data rows (`How::Data`) one system's table may hold: [`Table::bind`] keeps each
/// row's address in a fixed array, and refuses a table with more.
pub const MOST_DATA_ROWS: usize = 4;

/// A compile-time run's binding (D175): every name through blaze-comptime-sys's table, for the
/// system the code was built for (rule 75: `os` is handed in, never read from the host).
///
/// THREADS: the layer's one cell (blaze-comptime-sys child.rs `LAYER`) is touched only by the
/// thread that installed it and, once `run` starts, by the program's thread. The binder runs on
/// `run`'s CONTROL thread, so it must never touch the cell. Only a data row's binding does
/// (`environ`'s pointer array is built in the layer), so [`Table::bind`] binds every data row
/// of `os` on the installing thread BEFORE `run` starts its threads (the spawn orders those
/// writes before every read), and [`Bind::address`] answers a data row from that list. Every
/// other row is answered from constants (`find`, `function`) or the process's own `dlsym`.
/// This is by construction: a `Table` exists only through `bind`.
#[derive(Clone, Copy, Debug)]
pub struct Table {
    os: Os,
    /// Each data row of `os` and the address the layer gave it, `count` of them.
    data: [(&'static str, usize); MOST_DATA_ROWS],
    count: usize,
}

/// Why a [`Table`] could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableError {
    /// The table of `os` holds more data rows than [`MOST_DATA_ROWS`].
    TooManyData { os: Os, most: usize },
    /// The layer gave no address for data row `name` (it has no word for it: a bug of
    /// blaze-comptime-sys, whose test checks every row has one).
    DataUnbound { name: &'static str, why: Unbound },
}

impl core::fmt::Display for TableError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TableError::TooManyData { os, most } => write!(
                f,
                "blaze-comptime-sys's table for {os:?} holds more than {most} data rows (run_mode/bind.rs `MOST_DATA_ROWS`)"
            ),
            TableError::DataUnbound { name, why } => {
                write!(f, "blaze-comptime-sys's layer gives no word for its data row `{name}` ({why:?}; child.rs `data`)")
            }
        }
    }
}

impl Table {
    /// The table of `os`, its data rows bound now, on the calling thread: call it on the thread
    /// that installed the layer, after `install` and before `run` (the struct's doc).
    pub fn bind(os: Os) -> Result<Table, TableError> {
        let mut table = Table { os, data: [("", 0); MOST_DATA_ROWS], count: 0 };
        for row in NAMES.iter().filter(|row| row.how == How::Data && row.on.has(os)) {
            if table.count == MOST_DATA_ROWS {
                return Err(TableError::TooManyData { os, most: MOST_DATA_ROWS });
            }
            let at = match binding(row.c, os) {
                Ok(Binding::Here(at)) => at,
                Ok(Binding::Own) => return Err(TableError::DataUnbound { name: row.c, why: Unbound::NotInTable }),
                Err(refused) => return Err(TableError::DataUnbound { name: row.c, why: refused.why }),
            };
            table.data[table.count] = (row.c, at);
            table.count += 1;
        }
        Ok(table)
    }

    /// The table of `os` with NO data row bound (a test with no layer installed): a data row
    /// asked of it is refused by [`Bind::address`].
    #[cfg(test)]
    pub(crate) fn no_data(os: Os) -> Table {
        Table { os, data: [("", 0); MOST_DATA_ROWS], count: 0 }
    }

    /// The system this table answers for.
    pub fn os(&self) -> Os {
        self.os
    }
}

impl Bind for Table {
    fn address(&mut self, name: &str, cname: &CStr) -> Result<u64, &'static str> {
        // A data row: the address `bind` took on the layer's thread; the cell is not touched.
        if let Some(&(_, at)) = self.data[..self.count].iter().find(|(row, _)| *row == name) {
            return Ok(at as u64);
        }
        if matches!(find(name, self.os), Ok(row) if row.how == How::Data) {
            return Err("a data row of blaze-comptime-sys's table not bound before the run (run_mode/bind.rs `Table::bind`)");
        }
        match binding(name, self.os) {
            Ok(Binding::Here(at)) => Ok(at as u64),
            // A row the table admits as the process's own (memory, strings, unwinding).
            Ok(Binding::Own) => Dlsym.address(name, cname),
            Err(refused) => Err(match refused.why {
                Unbound::NotInTable => {
                    "a compile-time program binds only the names blaze-comptime-sys's table answers (table.rs `NAMES`), and it has no row for this one"
                }
                Unbound::OtherSystem { .. } => "blaze-comptime-sys's table answers this name only on the other system",
            }),
        }
    }
}
