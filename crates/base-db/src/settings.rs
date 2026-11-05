//! What the front end is asked to do besides its inputs: the borrow checker's mode and the
//! checks and traces a developer turns on. Read from the process's variables once, as its
//! database is made ([`crate::Db::new`]), and held as an input, so no query reads the
//! environment and a value never depends on when its query ran.

use blaze_query::db::Durability;

use crate::Db;

/// Which borrow checker runs: `BLAZE_BORROWCK`, `polonius` (the default, nightly Rust's
/// `-Zpolonius=next`, which accepts more), `polonius-flow` (opt-in: a body the default rejects is
/// checked again with datalog Polonius's kills and subsets, and accepted where those prove it
/// sound; it only ever accepts more than `polonius`), `nll` or `off`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BorrowckMode {
    Nll,
    Polonius,
    PoloniusFlow,
    Off,
}

impl BorrowckMode {
    /// The mode a value of `BLAZE_BORROWCK` names; unset or empty is the default.
    ///
    /// # Panics
    ///
    /// On any other value, naming it and the ones accepted: a misspelled mode never quietly
    /// checks with the default.
    pub fn from_env_value(value: Option<&str>) -> BorrowckMode {
        match value {
            None | Some("" | "polonius") => BorrowckMode::Polonius,
            Some("polonius-flow") => BorrowckMode::PoloniusFlow,
            Some("nll") => BorrowckMode::Nll,
            Some("off" | "0") => BorrowckMode::Off,
            Some(other) => panic!(
                "BLAZE_BORROWCK={other:?} names no borrow checker: use `polonius` (the default), \
                 `polonius-flow`, `nll` or `off`"
            ),
        }
    }
}

/// The front end's settings: one per database, set as it is made.
#[blaze_query::input(singleton, debug)]
pub struct FrontSettings {
    #[returns(copy)]
    pub borrowck: BorrowckMode,
    /// `BLAZE_BORROWCK_DEBUG`: `1` prints the regions, constraints and loans of every body
    /// checked, a name those of the functions called so and their closures.
    #[returns(ref)]
    pub borrowck_debug: Option<Box<str>>,
    /// `BLAZE_TRACE_HOLDERS=1`: method resolution writes why each holder of a module makes the
    /// maintainer lower its bodies.
    #[returns(copy)]
    pub trace_holders: bool,
    /// `RA_CHECK_RELATIVE_BODIES`: each body and block lowered alone is checked against the
    /// same lowered in its file, and no tree is kept.
    #[returns(copy)]
    pub check_relative_bodies: bool,
}

/// The settings the process's variables ask for.
pub(crate) fn set_from_env(db: &mut Db) {
    let borrowck = BorrowckMode::from_env_value(std::env::var("BLAZE_BORROWCK").ok().as_deref());
    // alloc: once per database
    let borrowck_debug = std::env::var("BLAZE_BORROWCK_DEBUG").ok().map(String::into_boxed_str);
    let trace_holders = std::env::var_os("BLAZE_TRACE_HOLDERS").is_some_and(|it| it == "1");
    let check_relative_bodies = std::env::var_os("RA_CHECK_RELATIVE_BODIES").is_some();
    _ = FrontSettings::new(
        db,
        Durability::HIGH,
        borrowck,
        borrowck_debug,
        trace_holders,
        check_relative_bodies,
    );
}
