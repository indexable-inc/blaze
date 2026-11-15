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
///
/// Read by field: a getter lists a fact of its own field alone ([`SettingField`]), never the
/// singleton's one fact, so a step that reads `borrowck_debug` depends on that setting and is
/// not moved by another (`parts`: the raw getters list nothing, and each public getter below
/// lists its field's fact before it reads it).
#[blaze_memo::input(singleton, debug, parts)]
pub struct FrontSettings {
    #[returns(copy)]
    #[get(borrowck_raw)]
    borrowck: BorrowckMode,
    #[returns(ref)]
    #[get(borrowck_debug_raw)]
    borrowck_debug: Option<Box<str>>,
    #[returns(copy)]
    #[get(trace_holders_raw)]
    trace_holders: bool,
    #[returns(copy)]
    #[get(check_relative_bodies_raw)]
    check_relative_bodies: bool,
}

/// One field of [`FrontSettings`], as a read of it is listed: a `Read::Input` row whose kind is
/// the field's own ([`SettingField::kind`]) and whose fact is the field's name and value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingField {
    Borrowck,
    BorrowckDebug,
    TraceHolders,
    CheckRelativeBodies,
}

static BORROWCK_FIELD: blaze_memo::Kind =
    blaze_memo::Kind::new("base_db::settings::FrontSettings::borrowck", blaze_memo::CycleKind::Panic);
static BORROWCK_DEBUG_FIELD: blaze_memo::Kind = blaze_memo::Kind::new(
    "base_db::settings::FrontSettings::borrowck_debug",
    blaze_memo::CycleKind::Panic,
);
static TRACE_HOLDERS_FIELD: blaze_memo::Kind = blaze_memo::Kind::new(
    "base_db::settings::FrontSettings::trace_holders",
    blaze_memo::CycleKind::Panic,
);
static CHECK_RELATIVE_BODIES_FIELD: blaze_memo::Kind = blaze_memo::Kind::new(
    "base_db::settings::FrontSettings::check_relative_bodies",
    blaze_memo::CycleKind::Panic,
);

/// The domain of a field's fact (its id is the field's name, its digest the field's value).
const FIELD_DOMAIN: &str = "blaze v2 front setting field 1";

impl SettingField {
    pub const ALL: [SettingField; 4] = [
        SettingField::Borrowck,
        SettingField::BorrowckDebug,
        SettingField::TraceHolders,
        SettingField::CheckRelativeBodies,
    ];

    /// The field's name, as `FrontSettings` spells it.
    pub const fn name(self) -> &'static str {
        match self {
            SettingField::Borrowck => "borrowck",
            SettingField::BorrowckDebug => "borrowck_debug",
            SettingField::TraceHolders => "trace_holders",
            SettingField::CheckRelativeBodies => "check_relative_bodies",
        }
    }

    /// The kind a read row of this field carries.
    pub fn kind(self) -> blaze_memo::KindId {
        blaze_memo::KindId::of(match self {
            SettingField::Borrowck => &BORROWCK_FIELD,
            SettingField::BorrowckDebug => &BORROWCK_DEBUG_FIELD,
            SettingField::TraceHolders => &TRACE_HOLDERS_FIELD,
            SettingField::CheckRelativeBodies => &CHECK_RELATIVE_BODIES_FIELD,
        })
    }

    /// The field a read row's kind is of, `None` for a kind of no field of `FrontSettings`.
    pub fn of_kind(kind: blaze_memo::KindId) -> Option<SettingField> {
        SettingField::ALL.into_iter().find(|it| it.kind() == kind)
    }
}

/// Lists the read of `field`, whose value's bytes are `value`: one fact per field, its id the
/// field's name and its digest the value, so two reads of one field in one snapshot are one
/// fact with one digest.
fn list<D: ?Sized + blaze_memo::Database>(db: &D, field: SettingField, value: &[u8]) {
    let fact = blaze_memo::Fact {
        id: ContentId::of(FIELD_DOMAIN, field.name().as_bytes()),
        digest: Hasher::new(FIELD_DOMAIN).part(field.name().as_bytes()).part(value).finish(),
    };
    blaze_memo::read(
        blaze_memo::Database::memo(db),
        blaze_memo::Read::Input { kind: field.kind(), fact },
    );
}

impl FrontSettings {
    /// Which borrow checker runs (`BLAZE_BORROWCK`); lists [`SettingField::Borrowck`].
    pub fn borrowck<D: ?Sized + blaze_memo::Database>(self, db: &D) -> BorrowckMode {
        let it = self.borrowck_raw(db);
        list(db, SettingField::Borrowck, &[mode_byte(it)]);
        it
    }

    /// `BLAZE_BORROWCK_DEBUG`: `1` prints the regions, constraints and loans of every body
    /// checked, a name those of the functions called so and their closures. Lists
    /// [`SettingField::BorrowckDebug`].
    pub fn borrowck_debug<'db, D: ?Sized + blaze_memo::Database>(
        self,
        db: &'db D,
    ) -> &'db Option<Box<str>> {
        let it = self.borrowck_debug_raw(db);
        match it.as_deref() {
            None => list(db, SettingField::BorrowckDebug, &[0]),
            Some(text) => {
                let mut bytes = [0u8; 33];
                bytes[0] = 1;
                bytes[1..].copy_from_slice(&ContentId::of(FIELD_DOMAIN, text.as_bytes()).0);
                list(db, SettingField::BorrowckDebug, &bytes);
            }
        }
        it
    }

    /// `BLAZE_TRACE_HOLDERS=1`: method resolution writes why each holder of a module makes the
    /// maintainer lower its bodies. Lists [`SettingField::TraceHolders`].
    pub fn trace_holders<D: ?Sized + blaze_memo::Database>(self, db: &D) -> bool {
        let it = self.trace_holders_raw(db);
        list(db, SettingField::TraceHolders, &[u8::from(it)]);
        it
    }

    /// `RA_CHECK_RELATIVE_BODIES`: each body and block lowered alone is checked against the
    /// same lowered in its file, and no tree is kept. Lists
    /// [`SettingField::CheckRelativeBodies`].
    pub fn check_relative_bodies<D: ?Sized + blaze_memo::Database>(self, db: &D) -> bool {
        let it = self.check_relative_bodies_raw(db);
        list(db, SettingField::CheckRelativeBodies, &[u8::from(it)]);
        it
    }
}

/// A borrow checker mode's byte, in the settings' content and a field's fact.
fn mode_byte(mode: BorrowckMode) -> u8 {
    match mode {
        BorrowckMode::Nll => 0,
        BorrowckMode::Polonius => 1,
        BorrowckMode::PoloniusFlow => 2,
        BorrowckMode::Off => 3,
    }
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
        let mode = mode_byte(self.borrowck);
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
