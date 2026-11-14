//! What the front end is asked to do besides its inputs: the borrow checker's mode and the
//! checks and traces a developer turns on. Read from the process's variables once, as its
//! database is made ([`crate::Db::new`]), kept by the world as [`FrontSettingsData`] and made
//! the session arena's singleton [`FrontSettings`] each time that arena is made, so no query
//! reads the environment and a value never depends on when its query ran.
//!
//! Cost (rule 67): a set that changes nothing marks nothing; one that changes a field marks the
//! session arena, which drops every arena (the floor: every query may read a setting).

use blaze_ids::{ContentId, Hasher};

use crate::content::InputId;

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

/// The front end's settings: the session arena's singleton, made from the world's
/// [`FrontSettingsData`].
#[blaze_memo::input(singleton, debug)]
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

/// The world's copy of [`FrontSettings`]: what [`crate::Edit::set_settings`] takes and the
/// session arena's singleton is made from.
#[derive(Clone, Debug, PartialEq, Eq)] // clone: once per database, and per set of the settings
pub struct FrontSettingsData {
    pub borrowck: BorrowckMode,
    pub borrowck_debug: Option<Box<str>>,
    pub trace_holders: bool,
    pub check_relative_bodies: bool,
}

/// The domain of the [`FrontSettings`] singleton's fact.
const SETTINGS_DOMAIN: &str = "blaze v2 front settings 1";

impl FrontSettingsData {
    /// The settings the process's variables ask for.
    pub fn from_env() -> FrontSettingsData {
        let borrowck =
            BorrowckMode::from_env_value(std::env::var("BLAZE_BORROWCK").ok().as_deref());
        // Once per database.
        let borrowck_debug =
            std::env::var("BLAZE_BORROWCK_DEBUG").ok().map(String::into_boxed_str);
        let trace_holders = std::env::var_os("BLAZE_TRACE_HOLDERS").is_some_and(|it| it == "1");
        let check_relative_bodies = std::env::var_os("RA_CHECK_RELATIVE_BODIES").is_some();
        FrontSettingsData { borrowck, borrowck_debug, trace_holders, check_relative_bodies }
    }

    /// The content ID of the settings: what the singleton's fact's digest is.
    pub(crate) fn content(&self) -> ContentId {
        let mut hasher = Hasher::new(SETTINGS_DOMAIN);
        let mode = match self.borrowck {
            BorrowckMode::Nll => 0,
            BorrowckMode::Polonius => 1,
            BorrowckMode::PoloniusFlow => 2,
            BorrowckMode::Off => 3,
        };
        hasher
            .update(&[mode, u8::from(self.trace_holders), u8::from(self.check_relative_bodies)])
            .opt_part(self.borrowck_debug.as_deref());
        hasher.finish()
    }

    /// Makes the session arena's [`FrontSettings`] of these settings.
    pub(crate) fn set_in(&self, inputs: &blaze_memo::Inputs<'_>) {
        let fact = crate::world::facts::singleton(SETTINGS_DOMAIN, InputId(self.content()));
        FrontSettings::set(
            inputs,
            fact,
            self.borrowck,
            // clone: the settings' one string, once per making of the session arena
            self.borrowck_debug.clone(),
            self.trace_holders,
            self.check_relative_bodies,
        );
    }
}
