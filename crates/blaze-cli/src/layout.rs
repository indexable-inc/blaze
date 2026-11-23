//! THE LAYOUT (rule 30): every folder blaze writes beneath a home's `.blaze` and a checkout's
//! `.blaze`, one typed row each, saying what it holds, whether a later command makes it again
//! when it is gone, and which `blaze clean` scope removes it, or why none does. Every scope of
//! `blaze clean` reads its folders from here ([`CHECKOUT`], [`STORE`], [`of`]`(Scope::All)`) and
//! removes only these rows' folders, each by its exact path beneath the home or checkout it was
//! given, never a name it matched (clean.rs `walk`). A test holds every writer's path constant
//! to a row, so a folder a writer names is one this table says something of.
//!
//! What stays a folder (claim B (b) of the 2026-11-21 lane, decided: no row below is folded
//! into the store): the sources, the toolchain shelf, the native work, `runs` and the test
//! folders are read by paths handed to other programs (a compiler, a build script, the viewer,
//! a test), which a store record is not; the expansions, the SDK answers and the roster are each
//! read by a reader of its own form, and moving one is that reader's change, not clean's.
//!
//! Bounds (rule 18) of the two rows that grow per command: none exists in the writer today,
//! said here rather than hidden; until one does, `blaze clean --all` removes each whole.
//! - `test/tmp`: each `blaze test` makes `<pid>-<ns>/<suite>` (test_drive.rs `TEMPS`) and does
//!   not remove it at its end. The writer's fix: the command removes its own `<pid>-<ns>`, by
//!   exact path, once every suite has ended.
//! - `native-work`: one folder a command name (`native-work/blaze`), with `pkg/<key>`,
//!   `out/<key>` and `deps/<key>` made for each native unit run (blaze-proc-macro
//!   native_unit.rs) and never removed, so it grows by one key a native unit's change. The
//!   writer's fix (blaze-proc-macro): a run's folders removed once its outputs are records.

use core::fmt;

use crate::clean::Place;

/// Which `blaze clean` removes a row's folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// `blaze clean`: the checkout's `.blaze/v2` whole, once each name in it is one of
    /// clean.rs `CheckoutEntry`.
    Checkout,
    /// `blaze clean --store` (and `--all`): the home's store root whole (with no flag, only its
    /// stores of other forms: clean.rs `remove_other`).
    Store,
    /// `blaze clean --all`: each of these folders whole, by its exact path. Rebuildable home
    /// folders only (the const check below).
    All,
    /// No clean removes it, for the reason said; `blaze clean --all` prints it beside each kept
    /// folder it finds.
    Kept(Why),
}

/// Why no clean removes a row's folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// Made by the bootstrap builder: no command makes it again.
    Bootstrap,
    /// Read by the store's collector to keep what the binaries that ran use.
    Collector,
    /// A live helper listens in it.
    LiveHelper,
    /// Fetched over the network: a clean never makes the next command fetch it again.
    Fetched,
    /// History no command makes again.
    History,
    /// The checkout's ignore file: not state.
    Ignore,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Why::Bootstrap => "made by the bootstrap builder, no command rebuilds it",
            Why::Collector => "the store's collector reads it to keep what the binaries that ran use",
            Why::LiveHelper => "a live journal helper's socket is in it",
            Why::Fetched => "fetched over the network; a clean never makes the next command fetch it again",
            Why::History => "each run's telemetry, history no command makes again",
            Why::Ignore => "keeps blaze's folder out of the checkout's git status; not state",
        })
    }
}

/// What a row names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    Folder,
    File,
}

/// One row of the layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub place: Place,
    /// Beneath the place's root; it begins `.blaze/`.
    pub path: &'static str,
    pub shape: Shape,
    /// What it holds, and its writer: printed by `blaze clean --all` beside each folder it
    /// removes.
    pub holds: &'static str,
    /// Whether a later command makes it again when it is gone (fetched again, or derived).
    pub rebuildable: bool,
    pub scope: Scope,
}

impl Row {
    /// The path beneath the home's `.blaze/v2` (or the checkout's), as `clean` names a kept one.
    pub fn short(&self) -> &'static str {
        self.path.strip_prefix(".blaze/v2/").unwrap_or(self.path)
    }
}

const fn home(path: &'static str, holds: &'static str, rebuildable: bool, scope: Scope) -> Row {
    Row { place: Place::Home, path, shape: Shape::Folder, holds, rebuildable, scope }
}

/// The checkout's state: `blaze clean`'s one folder (clean.rs `checkout_state`).
pub const CHECKOUT: Row = Row {
    place: Place::Checkout,
    path: crate::clean::CHECKOUT_STATE,
    shape: Shape::Folder,
    holds: "the checkout's own state: the profiles, the old pointers, packs and forms, the file watch's cookies (clean.rs `CheckoutEntry`)",
    rebuildable: true,
    scope: Scope::Checkout,
};

/// The home's store root: `blaze clean --store`'s one folder (clean.rs `store`).
pub const STORE: Row = home(crate::clean::HOME_STORE, "the store root: this blaze's store and other forms' (store/plain_place.rs)", true, Scope::Store);

/// THE TABLE. A row's path is its writer's constant where the writer names it whole, else the
/// writer's parts joined (the test below holds each equal to its writer's).
pub const LAYOUT: [Row; 22] = [
    CHECKOUT,
    Row {
        place: Place::Checkout,
        path: crate::sweep::HOME_IGNORE,
        shape: Shape::File,
        holds: "`*`, keeping blaze's folder out of the checkout's git status (print.rs `ensure_checkout_ignore`)",
        rebuildable: true,
        scope: Scope::Kept(Why::Ignore),
    },
    STORE,
    home(crate::toolchain::SHELF, "the toolchain shelf: the pinned std records and clang (toolchain.rs; blaze-proc-macro native_setting.rs `SHELF_DIR`)", true, Scope::Kept(Why::Fetched)),
    home(crate::fetch::SOURCES, "the locked dependency sources, one record a unit (fetch.rs)", true, Scope::Kept(Why::Fetched)),
    home(crate::sdk::ANSWERS, "xcrun's answers, one file a key (sdk.rs)", true, Scope::All),
    home(crate::run_parent::RECORDS, "the code records a run's launcher reads, and its native libraries (run_parent.rs `RecordsDir`)", true, Scope::All),
    home(".blaze/v2/runs", "each run's telemetry and its viewer's files (own_process.rs, blaze-launch-rt viewer.rs)", false, Scope::Kept(Why::History)),
    home(crate::test_drive::TEMPS, "each `blaze test` command's test folders, `<pid>-<ns>/<suite>`, which it leaves behind (layout.rs)", true, Scope::All),
    home(blaze_proc_macro::native_setting::NATIVE_WORK_DIR, "native unit runs' work folders, `<command>/{pkg,out,deps}/<key>`, never removed by a run (layout.rs)", true, Scope::All),
    home(".blaze/v2/proc-macro/modules", "proc macros' wasm modules (macros.rs `MODULE_LISTING`)", false, Scope::Kept(Why::Bootstrap)),
    home(".blaze/v2/proc-macro/compiled", "proc-macro modules compiled for Wasmtime (blaze-proc-macro module.rs `COMPILED_DIR`)", true, Scope::All),
    home(".blaze/v2/proc-macro/expansions", "proc-macro expansion records (proc_runner.rs `EXPANSIONS`)", true, Scope::All),
    home(".blaze/v2/build-script/modules", "build scripts' wasm modules (scripts.rs `MODULE_LISTING`)", false, Scope::Kept(Why::Bootstrap)),
    home(".blaze/v2/build-script/compiled", "build-script modules compiled for Wasmtime (blaze-proc-macro script.rs)", true, Scope::All),
    home(".blaze/v2/build-script/records", "build-script run records (blaze-proc-macro record.rs)", true, Scope::All),
    home(".blaze/v2/build-script/files", "build scripts' output files (blaze-build-script outputs.rs `FILE_AREA`)", true, Scope::All),
    home(".blaze/v2/roster", "the binaries that ran in the store's window (roster.rs)", false, Scope::Kept(Why::Collector)),
    home(
        ".blaze/v2/journal",
        "the journal helpers' sockets, `<16 hex>.sock`, in a folder made the user's alone (mode 0700; blaze-watch helper.rs `SOCKET_FOLDER`)",
        true,
        Scope::Kept(Why::LiveHelper),
    ),
    home(
        ".blaze/v2/daemon",
        "the resident per-checkout daemons' sockets, `<16 hex>.sock`, in a folder made the user's alone (mode 0700; blaze-watch daemon.rs `SOCKET_FOLDER`)",
        true,
        Scope::Kept(Why::LiveHelper),
    ),
    home(".blaze/v2/shared", "an old home's store (store.rs `OLD_HOME`): reported, never read", true, Scope::All),
    home(".blaze/v2/own", "an old home's store (store.rs `OLD_HOME`): reported, never read", true, Scope::All),
];

// The table's promises, checked when it compiles (rule 76): `--all` removes only rebuildable
// home folders; [`CHECKOUT`] and [`STORE`] are the one row of their scope; every path is
// beneath blaze's folder.
const _: () = {
    let (mut checkouts, mut stores, mut i) = (0, 0, 0);
    while i < LAYOUT.len() {
        let row = &LAYOUT[i];
        if let Scope::All = row.scope {
            assert!(row.rebuildable && matches!(row.place, Place::Home) && matches!(row.shape, Shape::Folder), "an `--all` row must be a rebuildable home folder");
        }
        if let Scope::Checkout = row.scope {
            checkouts += 1;
        }
        if let Scope::Store = row.scope {
            stores += 1;
        }
        let path = row.path.as_bytes();
        assert!(path.len() > 7 && path[0] == b'.' && path[1] == b'b' && path[6] == b'/', "a row's path is beneath `.blaze/`");
        i += 1;
    }
    assert!(checkouts == 1 && stores == 1, "one row of the checkout's scope, one of the store's");
};

/// The rows `scope` names, in the table's order.
pub fn of(scope: Scope) -> impl Iterator<Item = &'static Row> {
    LAYOUT.iter().filter(move |row| row.scope == scope)
}

/// The home's rows no clean removes, each with why, in the table's order.
pub fn kept_at_home() -> impl Iterator<Item = (&'static Row, Why)> {
    LAYOUT.iter().filter_map(|row| match (row.place, row.scope) {
        (Place::Home, Scope::Kept(why)) => Some((row, why)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row `path` (beneath `place`'s root) is, or is beneath.
    fn row_of(place: Place, path: &str) -> Option<&'static Row> {
        LAYOUT.iter().find(|row| row.place == place && beneath(path, row.path))
    }

    fn beneath(path: &str, row: &str) -> bool {
        path == row || path.strip_prefix(row).is_some_and(|rest| rest.starts_with('/'))
    }

    /// Every writer's path constant is a row (the shelf's clang beneath one); no two rows of
    /// one place are one path or one beneath another; the checkout's names are beneath its
    /// state's row; a live helper's socket is kept.
    #[test]
    fn every_writers_path_is_a_row() {
        let blaze = crate::store::HOME_FOLDER;
        let v2 = crate::scripts::STORE_UNDER_HOME;
        let home_paths = [
            std::string::String::from(crate::clean::HOME_STORE),
            std::format!("{blaze}/{}", crate::store::plain_place::STORE_ROOT),
            std::string::String::from(crate::toolchain::SHELF),
            std::string::String::from(crate::fetch::SOURCES),
            std::string::String::from(crate::sdk::ANSWERS),
            std::string::String::from(crate::run_parent::RECORDS),
            std::string::String::from(crate::test_drive::TEMPS),
            std::string::String::from(blaze_proc_macro::native_setting::NATIVE_WORK_DIR),
            std::format!("{blaze}/{}", crate::macros::MODULE_LISTING),
            std::format!("{v2}/{}", blaze_proc_macro::module::COMPILED_DIR),
            std::format!("{v2}/{}", crate::proc_runner::EXPANSIONS),
            std::string::String::from(crate::proc_runner::ANSWERS_STORE.strip_prefix("<home>/").expect("the expansions' words begin `<home>/`")),
            std::format!("{blaze}/{}", crate::scripts::MODULE_LISTING),
            std::format!("{v2}/{}", blaze_build_script::outputs::FILE_AREA),
            std::format!("{blaze}/{}", crate::roster::ROSTER),
            std::format!("{blaze}/{}", blaze_watch::helper::SOCKET_FOLDER),
            std::format!("{blaze}/{}", crate::store::OLD_HOME[0]),
            std::format!("{blaze}/{}", crate::store::OLD_HOME[1]),
        ];
        for path in home_paths.iter() {
            let row = row_of(Place::Home, path).unwrap_or_else(|| panic!("the home's {path} is written and in no row"));
            assert_eq!(row.path, path.as_str(), "the home's {path} is beneath a row, not one");
        }
        let clang = row_of(Place::Home, blaze_proc_macro::native_setting::SHELF_DIR).expect("the shelf's clang");
        assert_eq!(clang.path, crate::toolchain::SHELF, "the clang shelf is not beneath the toolchain shelf");
        for path in [crate::clean::CHECKOUT_STATE, crate::sweep::HOME_IGNORE] {
            assert_eq!(row_of(Place::Checkout, path).map(|row| row.path), Some(path), "the checkout's {path} is written and in no row");
        }
        for entry in crate::clean::CheckoutEntry::ALL {
            assert_eq!(row_of(Place::Checkout, entry.path()).map(|row| row.scope), Some(Scope::Checkout), "{entry:?} is not beneath the checkout's state");
        }
        for (i, row) in LAYOUT.iter().enumerate() {
            for other in &LAYOUT[i + 1..] {
                let overlap = beneath(row.path, other.path) || beneath(other.path, row.path);
                assert!(row.place != other.place || !overlap, "{} and {} overlap", row.path, other.path);
            }
        }
        assert_eq!(of(Scope::Checkout).map(|row| row.path).collect::<std::vec::Vec<_>>(), [crate::clean::CHECKOUT_STATE]);
        assert_eq!(of(Scope::Store).map(|row| row.path).collect::<std::vec::Vec<_>>(), [crate::clean::HOME_STORE]);
        let journal = row_of(Place::Home, &std::format!("{blaze}/{}/0123456789abcdef.sock", blaze_watch::helper::SOCKET_FOLDER)).expect("the journal's socket");
        assert_eq!(journal.scope, Scope::Kept(Why::LiveHelper), "a live helper's socket would be removed");
        for row in LAYOUT.iter() {
            std::println!("{:?} {} {:?} rebuildable={}: {}", row.place, row.path, row.scope, row.rebuildable, row.holds);
        }
    }

    /// A home written by the previous layout (before `.blaze/v2/daemon` existed) still has every
    /// pre-existing row accounted for, and the new `.blaze/v2/daemon` row is `LiveHelper` (never
    /// required to exist and never wiped while a daemon is live).
    #[test]
    fn previous_layout_without_daemon_folder_is_handled() {
        let blaze = crate::store::HOME_FOLDER;
        let daemon = row_of(
            Place::Home,
            &std::format!("{blaze}/v2/{}/0123456789abcdef.sock", blaze_watch::daemon::SOCKET_FOLDER),
        )
        .expect("the daemon's socket row");
        assert_eq!(daemon.scope, Scope::Kept(Why::LiveHelper));
        for prev in [
            ".blaze/v2/journal/0123456789abcdef.sock",
            ".blaze/v2/shared",
            ".blaze/v2/own",
            ".blaze/v2/roster",
        ] {
            assert!(row_of(Place::Home, prev).is_some(), "previous layout path {prev} must stay recognized");
        }
    }
}
