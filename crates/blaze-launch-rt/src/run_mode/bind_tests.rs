//! The binder seam's two strategies (run_mode/bind.rs, rule 78), each instantiation tested
//! against the other where they must agree: a name blaze-comptime-sys's table answers `Own`
//! (`memcpy`, `malloc`) binds to the same address under [`Table`] as under [`Dlsym`]; a name the
//! table has no row for (`getpriority`) is refused by [`Table`] naming the table, while [`Dlsym`] binds it (the
//! control: the refusal is the table's, not a name this process lacks); a name no image defines
//! is refused by both.

use core::ffi::CStr;

use blaze_comptime_sys::Os;

use super::bind::{Bind, Dlsym, Table};

/// The system the test process runs on: the table is asked for the host's own rows.
const HOST: Os = if cfg!(target_os = "macos") { Os::Mac } else { Os::Linux };

fn both(name: &str, cname: &CStr) -> (Result<u64, &'static str>, Result<u64, &'static str>) {
    // No layer is installed here, so no data row is bound: the names these tests ask are rows
    // of other kinds (a data row asked of this table is refused, never bound off its thread).
    (Dlsym.address(name, cname), Table::no_data(HOST).address(name, cname))
}

/// Prints "bind: Own rows equal under both; a row-less name refused by the table only; an
/// undefined name refused by both".
#[test]
fn the_two_strategies_agree_on_own_rows_and_the_table_walls_the_rest() {
    for (name, cname) in [("memcpy", c"memcpy"), ("malloc", c"malloc")] {
        let (plain, table) = both(name, cname);
        assert!(plain.is_ok(), "this process defines `{name}`");
        assert_eq!(plain, table, "an `Own` row binds where an ordinary run binds `{name}`");
    }
    // `getpriority` is in every C library and in no row of the table: a compile-time program
    // reaches only what the table answers.
    let (plain, table) = both("getpriority", c"getpriority");
    assert!(plain.is_ok(), "the control: this process defines `getpriority`");
    assert!(matches!(table, Err(why) if why.contains("table.rs `NAMES`")), "the table walls `getpriority`: {table:?}");
    let (plain, table) = both("blaze_no_such_c_name", c"blaze_no_such_c_name");
    assert_eq!(plain, Err("no image of this process defines it (`dlsym`)"));
    assert!(table.is_err(), "nor does the table bind it");
    println!("bind: Own rows equal under both; a row-less name refused by the table only; an undefined name refused by both");
}
